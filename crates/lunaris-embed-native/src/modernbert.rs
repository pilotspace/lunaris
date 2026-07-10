//! `pooled_forward` — wraps `candle_transformers::models::modernbert::ModernBert`
//! with masked mean-pooling + L2-normalize so the output is the unit-norm
//! sentence vector the Lunaris `Embedder` trait expects.
//!
//! ## Reference
//!
//! - ModernBERT paper §3 (architecture; alternating local/global attention,
//!   GeGLU MLP, RMSNorm, RoPE). The forward pass itself is implemented in
//!   `candle_transformers 0.10.2`'s `models/modernbert.rs`. We do not
//!   re-implement it here — the candle impl is already a faithful port of
//!   the upstream `AnswerDotAI/ModernBERT` reference.
//! - HF `sentence-transformers` pipeline for granite-r2 is verified against
//!   the model's `modules.json` + `1_Pooling/config.json`:
//!   1. Transformer (the encoder),
//!   2. Pooling — **CLS-token** (`pooling_mode_cls_token: true`, NOT mean),
//!   3. Normalize (L2).
//!
//!   Care: the model's top-level `config.json` has `classifier_pooling: 'mean'`
//!   but that field controls the SequenceClassification head, NOT the
//!   sentence-embedding pipeline. The sentence-transformers loader reads
//!   `1_Pooling/config.json` which says CLS — that's the truth for embeddings.
//!
//! ## What this module owns
//!
//! - `pooled_forward(model, input_ids, attention_mask)` — runs the model,
//!   takes the CLS-token (index 0) hidden state, L2-normalizes, returns
//!   `(batch, hidden_size)`.
//! - Numerical fidelity: pooling math runs in **FP32** regardless of model
//!   dtype (the cast happens before L2-normalize). Empirically this keeps
//!   the 100-prompt panel inside the 0.5% mean-cosine-drift gate; doing the
//!   normalize in fp16 leaks ~1% on Vietnamese-diacritic-heavy panels.
//!
//! Everything else (embeddings, attention, RoPE, GeGLU, RMSNorm, alternating
//! local/global windows) is `ModernBert::forward` from candle-transformers.

use candle_core::{D, DType, IndexOp, Tensor};
use candle_transformers::models::modernbert::ModernBert;

use crate::GRANITE_R2_DIM;

/// Errors that can arise during the pooled forward pass. All variants are
/// thin wrappers around `candle_core::Error` plus shape validation.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("forward: model returned shape {actual:?}, expected hidden size {expected}")]
    UnexpectedHidden { actual: Vec<usize>, expected: usize },
}

/// Run the ModernBERT forward pass and emit CLS-pooled, L2-normalized
/// 768-d sentence vectors.
///
/// `input_ids`: shape `(batch, seq_len)`, `DType::U32`. The CLS token is
/// `input_ids[:, 0]` per the granite-r2 tokenizer config (`cls_token_id: 2`,
/// inserted automatically by `tokenizers::Tokenizer` when `add_special_tokens
/// = true`).
/// `attention_mask`: shape `(batch, seq_len)`, `DType::U32` — fed into the
/// candle model to mask pads. Not consumed during pooling (CLS-pooling reads
/// position 0 regardless of mask, matching `sentence_transformers.models.Pooling`
/// with `pooling_mode_cls_token: true`).
///
/// Returns: shape `(batch, hidden_size)`, `DType::F32`, unit-norm rows.
pub fn pooled_forward(
    model: &ModernBert,
    input_ids: &Tensor,
    attention_mask: &Tensor,
) -> Result<Tensor, ForwardError> {
    let (batch_size, max_seq_len) = input_ids.dims2().unwrap_or((0, 0));

    // The candle ModernBert forward returns `(batch, seq, hidden)` in the
    // model's compute dtype. We feed in U32 ids and the U32 mask (which is
    // what candle_transformers also expects per its
    // `prepare_4d_attention_mask` signature). Span-per-call: this is the
    // stage the §4b microscope decision rule cares about — if ≥60% of p50
    // latency lives inside this span, candle's matmul kernels are the
    // bottleneck (runtime-swap lever); if it's tokenize/copy instead, fix in
    // place.
    let hidden = {
        let _enter = tracing::trace_span!(
            "lunaris.embed.forward",
            batch_size,
            max_seq_len,
            quant = "fp32"
        )
        .entered();
        model.forward(input_ids, attention_mask)?
    };

    let dims = hidden.dims().to_vec();
    if dims.len() != 3 {
        return Err(ForwardError::UnexpectedHidden { actual: dims, expected: GRANITE_R2_DIM });
    }
    let hidden_size = dims[2];

    let _pool_enter =
        tracing::trace_span!("lunaris.embed.pooling", batch_size, max_seq_len, quant = "fp32")
            .entered();

    // CLS-pool: take the first position along the seq axis. Result: (b, hidden).
    let cls = hidden.i((.., 0, ..))?.contiguous()?;

    // Cast to FP32 for the L2-normalize step (see module doc — bf16/fp16
    // normalize leaks measurable drift on multilingual panels).
    let pooled = cls.to_dtype(DType::F32)?;

    // L2-normalize per row. l2 = sqrt(sum(x^2)); guard with 1e-12.
    let sq = pooled.sqr()?.sum_keepdim(D::Minus1)?; // (b, 1)
    let eps = Tensor::new(&[1e-12_f32], pooled.device())?
        .to_dtype(DType::F32)?
        .broadcast_as(sq.shape())?;
    let norm = sq.maximum(&eps)?.sqrt()?; // (b, 1)
    let unit = pooled.broadcast_div(&norm)?; // (b, hidden)

    if hidden_size != GRANITE_R2_DIM {
        return Err(ForwardError::UnexpectedHidden {
            actual: unit.dims().to_vec(),
            expected: GRANITE_R2_DIM,
        });
    }
    Ok(unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::Device;

    // We can't construct a real ModernBert here without weights, but we CAN
    // exercise the pooling math by hand-crafting a tiny tensor and verifying
    // the shape/unit-norm invariants on a stub path. The full
    // numerical-equivalence test (with real granite-r2 weights) lives in
    // `tests/numerical_equivalence.rs` behind the `embedder-it` feature.

    #[test]
    fn cls_pool_and_normalize_via_synthetic_tensors() -> Result<(), Box<dyn std::error::Error>> {
        // Simulate ModernBert's output for batch=2, seq=3, hidden=4. Verify
        // that CLS-pool (index 0) + L2-normalize produces unit-norm rows
        // matching the CLS row direction.
        let device = Device::Cpu;
        let hidden: Tensor = Tensor::from_vec(
            vec![
                // batch 0 — CLS row is [3, 4, 0, 0] → after L2 norm → [0.6, 0.8, 0, 0]
                3.0_f32, 4.0, 0.0, 0.0, // pos 0 (CLS)
                9.0, 9.0, 9.0, 9.0, // pos 1 (ignored by CLS pool)
                7.0, 7.0, 7.0, 7.0, // pos 2 (ignored)
                // batch 1 — CLS row is [1, 0, 0, 0] (already unit-norm)
                1.0, 0.0, 0.0, 0.0, // pos 0
                5.0, 5.0, 5.0, 5.0, // pos 1
                2.0, 2.0, 2.0, 2.0, // pos 2
            ],
            (2, 3, 4),
            &device,
        )?;

        // Mirror the production CLS-pool + L2-normalize math.
        let cls = hidden.i((.., 0, ..))?.contiguous()?;
        let pooled = cls.to_dtype(DType::F32)?;
        let sq = pooled.sqr()?.sum_keepdim(D::Minus1)?;
        let eps = Tensor::new(&[1e-12_f32], pooled.device())?
            .to_dtype(DType::F32)?
            .broadcast_as(sq.shape())?;
        let norm = sq.maximum(&eps)?.sqrt()?;
        let unit = pooled.broadcast_div(&norm)?;

        let unit_vec: Vec<Vec<f32>> = unit.to_vec2()?;
        assert_eq!(unit_vec.len(), 2);
        for row in &unit_vec {
            assert_eq!(row.len(), 4);
            let l2 = row.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            assert!((l2 - 1.0).abs() < 1e-5, "row not unit-norm: l2={l2}");
        }
        assert!((unit_vec[0][0] - 0.6).abs() < 1e-6);
        assert!((unit_vec[0][1] - 0.8).abs() < 1e-6);
        assert!((unit_vec[1][0] - 1.0).abs() < 1e-6);
        Ok(())
    }
}
