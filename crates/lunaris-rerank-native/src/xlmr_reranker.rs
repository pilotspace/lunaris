//! `XlmRobertaReranker` — wraps candle's
//! `XLMRobertaForSequenceClassification` and emits sigmoid-bounded scores in
//! `[0, 1]` for `bge-reranker-v2-m3`.
//!
//! ## What this module owns
//!
//! - Construction from a `VarBuilder` + parsed config.
//! - The forward pass: `score(input_ids, attention_mask, token_type_ids)` →
//!   `Tensor` of shape `(batch,)`, `DType::F32`, values in `[0, 1]`.
//!
//! ## What we delegate to candle
//!
//! `XLMRobertaForSequenceClassification`'s forward pass already:
//! 1. Embeds + encodes with XLM-RoBERTa (24 layers × 16 heads × 1024 hidden).
//! 2. Selects position 0 (the `<s>`/CLS-equivalent) from the final hidden states.
//! 3. Applies the `XLMRobertaClassificationHead`: `Linear(1024→1024) → tanh →
//!    Linear(1024→num_labels)`.
//!
//! For `num_labels = 1` (bge-reranker) the output is `(batch, 1)`. We squeeze
//! to `(batch,)` and apply sigmoid in FP32 to get a calibrated `[0, 1]` score.
//!
//! ## Why sigmoid?
//!
//! sentence-transformers `CrossEncoder.predict()` returns raw logits by
//! default; downstream callers either threshold the logit or apply sigmoid.
//! v0.4 Lunaris standardises on the sigmoid-bounded score so RRF fusion sees
//! a value in the same `[0, 1]` band as cosine similarity from the embedder.
//! The numerical-equivalence fixture is generated with the same sigmoid step
//! applied to the Python reference — see
//! `scripts/spike-generate-reference-rerank-scores.py`.

use candle_core::{DType, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::XLMRobertaForSequenceClassification;

use crate::config::XlmRobertaRerankerConfig;

/// Errors raised during the reranker forward pass. Thin wrappers around
/// `candle_core::Error` plus shape validation.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("forward: model returned shape {actual:?}, expected (batch, {expected_labels})")]
    UnexpectedShape { actual: Vec<usize>, expected_labels: usize },
}

/// XLM-RoBERTa sequence-classification head wrapped in a sigmoid output. v0.4
/// `bge-reranker-v2-m3` instance: `num_labels = 1`.
pub struct XlmRobertaReranker {
    /// candle's sequence-classification model. Holds the XLM-R encoder + the
    /// classification head; both immutable after load. Concurrent forward
    /// passes are safe (each lives on its own `spawn_blocking` thread); the
    /// `&self` receiver on `forward` rules out per-call mutation.
    model: XLMRobertaForSequenceClassification,
    /// Number of output labels (1 for bge-reranker-v2-m3). Pinned for shape
    /// validation; we never branch on it at runtime.
    num_labels: usize,
}

impl XlmRobertaReranker {
    /// Construct from a candle `VarBuilder` reading the model's safetensors and
    /// the parsed config. The VarBuilder's prefix must be the safetensors
    /// root (so candle finds `roberta.embeddings.*` and `classifier.*`).
    pub fn load(vb: VarBuilder<'_>, cfg: &XlmRobertaRerankerConfig) -> Result<Self, ForwardError> {
        let candle_cfg = cfg.to_candle();
        let model = XLMRobertaForSequenceClassification::new(cfg.num_labels, &candle_cfg, vb)?;
        Ok(Self { model, num_labels: cfg.num_labels })
    }

    /// Run the forward pass on a pre-tokenized batch and return per-row scores
    /// in `[0, 1]` (sigmoid of the single logit).
    ///
    /// `input_ids`, `attention_mask`, `token_type_ids`: shape `(batch, seq)`,
    /// `DType::U32`. Returns: shape `(batch,)`, `DType::F32`.
    pub fn score(
        &self,
        input_ids: &Tensor,
        attention_mask: &Tensor,
        token_type_ids: &Tensor,
    ) -> Result<Tensor, ForwardError> {
        let (batch_size, max_seq_len) = input_ids.dims2().unwrap_or((0, 0));

        // logits: (batch, num_labels). For bge-reranker-v2-m3, num_labels == 1.
        // Span-per-call: the §4b microscope decision rule reads this stage —
        // if ≥60% of p50 latency lives here, candle's matmul kernels are the
        // bottleneck (runtime-swap lever, §4c); otherwise fix
        // tokenize/batch/copy in place.
        let logits = {
            let _enter = tracing::trace_span!(
                "lunaris.rerank.forward",
                batch_size,
                max_seq_len,
                quant = "fp32"
            )
            .entered();
            self.model.forward(input_ids, attention_mask, token_type_ids)?
        };
        let dims = logits.dims().to_vec();
        if dims.len() != 2 || dims[1] != self.num_labels {
            return Err(ForwardError::UnexpectedShape {
                actual: dims,
                expected_labels: self.num_labels,
            });
        }

        let _extract_enter = tracing::trace_span!(
            "lunaris.rerank.score_extract",
            batch_size,
            max_seq_len,
            quant = "fp32"
        )
        .entered();

        // Squeeze the trailing num_labels axis when it's 1 (bge case). For
        // num_labels > 1 we'd need to choose a class index; bge is single-label.
        let scalar = if self.num_labels == 1 { logits.i((.., 0))? } else { logits };

        // Upcast to FP32 for the sigmoid + return. The forward pass itself may
        // run in fp16/bf16 (whichever dtype the VarBuilder loaded) but the
        // final calibration step runs in fp32 to match the reference's sigmoid
        // path (`sentence-transformers` always sigmoids in fp32).
        let logits_f32 = scalar.to_dtype(DType::F32)?.contiguous()?;
        // Sigmoid: 1 / (1 + exp(-x)). candle ships `ops::sigmoid` via
        // `candle_nn::ops::sigmoid` since 0.10; use it if available, otherwise
        // hand-roll. The hand-rolled form is numerically equivalent (and
        // matches the Python reference, which also goes through libm).
        let scores = candle_nn::ops::sigmoid(&logits_f32)?;
        Ok(scores)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::Device;

    #[test]
    fn sigmoid_on_synthetic_logits_matches_closed_form() -> Result<(), Box<dyn std::error::Error>> {
        // We cannot construct a real XLMRobertaForSequenceClassification
        // without weights, but we can independently exercise the sigmoid path
        // — that's the only math `XlmRobertaReranker::score` performs on top
        // of candle's forward pass. The reference values below come from
        // running `sigmoid(x)` in python's `math.exp` / `1/(1+e^-x)`.
        let device = Device::Cpu;
        // logits: [-3.0, -1.0, 0.0, 1.0, 3.0] reshaped to (5, 1) to mirror
        // the (batch, num_labels=1) output of the classification head.
        let logits = Tensor::from_vec(vec![-3.0_f32, -1.0, 0.0, 1.0, 3.0], (5, 1), &device)?;
        let squeezed = logits.i((.., 0))?.contiguous()?;
        let logits_f32 = squeezed.to_dtype(DType::F32)?;
        let scores = candle_nn::ops::sigmoid(&logits_f32)?;
        let v: Vec<f32> = scores.to_vec1()?;
        let expected = [0.047_425_874, 0.268_941_42, 0.5, 0.731_058_6, 0.952_574_13];
        for (i, (got, exp)) in v.iter().zip(expected.iter()).enumerate() {
            assert!(
                (got - exp).abs() < 1e-6,
                "row {i}: sigmoid({}): got {got}, expected {exp}",
                [-3.0_f32, -1.0, 0.0, 1.0, 3.0][i]
            );
        }
        Ok(())
    }
}
