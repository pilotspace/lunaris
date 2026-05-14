//! `quantized_modernbert` — Q4_K_M ModernBERT forward pass for granite-r2.
//!
//! **Status (2026-05-14): SCAFFOLD ONLY.** The conversion gate is cleared
//! (`scripts/spike-convert-granite-r2-to-gguf.sh` ships a 240 MB Q4_K_M GGUF,
//! SHA-256 pinned in `lib.rs`), the tensor manifest is captured under
//! `.planning/phases/N-01-step-2-quantized-gguf/Q4_K_M.tensor-manifest.txt`,
//! and the structural traps are documented in
//! `.planning/phases/N-01-step-2-quantized-gguf/conversion-evidence.md`.
//! The forward-pass body is the next-session deliverable.
//!
//! ## Why a scaffold-then-port split
//!
//! Hand-rolling a quantized ModernBERT in candle is ≥ 500 LOC of careful
//! tensor wiring; even after the GGUF conversion + manifest, getting drift
//! to mean ≤ 1% / max ≤ 3% on a 100-prompt panel takes empirical iteration
//! (each layer's per-tensor dtype mix can shift the per-prompt residual).
//! Landing the gate-cleared conversion + a RED test in this session keeps
//! the next session focused on a single coherent thing — the port — with
//! every external dependency already pinned.
//!
//! ## Structural reference (read before implementing the body)
//!
//! - FP16 reference: `crate::modernbert::pooled_forward` (CLS pool +
//!   fp32 L2-normalize). The Q4 path's `pooled_forward` must produce the
//!   same output shape, dtype, and pooling math.
//! - candle-transformers' FP16 `ModernBert::forward` (in
//!   `~/.cargo/registry/.../candle-transformers-0.10.2/src/models/modernbert.rs`)
//!   is the structural skeleton to mirror.
//! - candle-transformers' `quantized_llama.rs` shows the `QMatMul` wiring
//!   pattern (load `QTensor` from `gguf_file::Content`, wrap in `QMatMul`,
//!   replace every `Linear` call).
//!
//! ## Per-tensor dtype traps (from conversion-evidence.md)
//!
//! GGUF Q4_K_M for granite-r2 mixes types per tensor:
//! - `*_norm.weight` → F32  (LayerNorm scales — not quantized)
//! - `attn_qkv` / `attn_output` / `ffn_up` → mostly Q4_K, some Q6_K per layer
//! - `ffn_down` → Q8_0 or Q5_0 per layer
//! - `token_embd` → Q6_K (NOT Q4_K)
//!
//! `QMatMul::from_qtensor` handles all of these uniformly, but the loader
//! must NOT assume a single dtype for any tensor class. Read
//! `qtensor.dtype()` and trust the file.

use std::path::Path;

use candle_core::{Device, Tensor};

use crate::config::ModernBertConfig;

/// Errors raised by the quantized path. Mirrors the structure of
/// `ForwardError` so callers can do uniform error mapping.
#[derive(Debug, thiserror::Error)]
pub enum QuantizedError {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("gguf: {reason}")]
    Gguf { reason: String },

    #[error("not-yet-implemented: {0}")]
    NotImplemented(&'static str),
}

/// Quantized ModernBERT model loaded from a GGUF file. The forward pass
/// produces the same `(batch, hidden)` CLS-pooled, fp32-normalized output
/// as the FP16 path (`crate::modernbert::pooled_forward`).
///
/// **Scaffold only.** `load` and `pooled_forward` return
/// `QuantizedError::NotImplemented` until the next session lands the port.
#[allow(dead_code)] // body lands in the follow-up port
pub struct QuantizedModernBert {
    cfg: ModernBertConfig,
    device: Device,
    // body lands next session — placeholder fields documented here:
    //
    //   token_embd:        QMatMul,           // Q6_K [768, vocab_size]
    //   token_embd_norm:   (Tensor, Tensor),  // F32  weight + bias
    //   layers:            Vec<Layer>,        // 22 layers
    //   final_norm:        (Tensor, Tensor),  // F32  weight + bias
    //
    // where `Layer` carries QMatMul handles for attn_qkv / attn_output /
    // ffn_up / ffn_down plus F32 norm tensors. See conversion-evidence.md
    // for the per-tensor dtype manifest and naming map.
}

impl QuantizedModernBert {
    /// Load the model from a GGUF file. The `cfg` must already be validated
    /// (use `ModernBertConfig::try_from_json_path`) — we do NOT re-parse
    /// `config.json` from GGUF metadata because (a) the FP16 path's
    /// `try_from_json_path` is the canonical loader and (b) the GGUF
    /// `pooling_type` field is misleading (says MEAN; sentence-transformers
    /// uses CLS — see conversion-evidence.md trap #1).
    pub fn load(
        _path: &Path,
        cfg: &ModernBertConfig,
        device: &Device,
    ) -> Result<Self, QuantizedError> {
        // Next session — the body:
        //   1. let mut f = std::fs::File::open(path)?;
        //   2. let content = candle_core::quantized::gguf_file::Content::read(&mut f)?;
        //   3. for each layer index `i` in 0..cfg.num_hidden_layers:
        //        load blk.{i}.attn_qkv.weight as QTensor → QMatMul
        //        load blk.{i}.attn_output.weight as QTensor → QMatMul
        //        load blk.{i}.ffn_up.weight as QTensor → QMatMul (fused gate+up)
        //        load blk.{i}.ffn_down.weight as QTensor → QMatMul
        //        load blk.{i}.attn_norm.weight as F32 Tensor (layers 1+)
        //        load blk.{i}.ffn_norm.weight as F32 Tensor
        //   4. load token_embd, token_embd_norm, output_norm.
        Ok(Self { cfg: cfg.clone(), device: device.clone() })
    }

    /// Run the forward pass on a tokenized batch and emit CLS-pooled,
    /// L2-normalized 768-d vectors. Signature MUST mirror
    /// `crate::modernbert::pooled_forward` so the integration test can swap
    /// implementations without ceremony.
    ///
    /// `input_ids`: `(batch, seq_len)` `DType::U32`.
    /// `attention_mask`: `(batch, seq_len)` `DType::U32`.
    /// Returns: `(batch, hidden_size)` `DType::F32`, unit-norm rows.
    pub fn pooled_forward(
        &self,
        _input_ids: &Tensor,
        _attention_mask: &Tensor,
    ) -> Result<Tensor, QuantizedError> {
        Err(QuantizedError::NotImplemented(
            "QuantizedModernBert::pooled_forward — next session port. See \
             .planning/phases/N-01-step-2-quantized-gguf/conversion-evidence.md \
             for tensor manifest, dtype map, and the six structural traps.",
        ))
    }

    /// Accessor for the loaded config — used by the embedder shim to wire
    /// the tokenizer's `max_position_embeddings` without re-parsing.
    pub fn config(&self) -> &ModernBertConfig {
        &self.cfg
    }

    /// Accessor for the model's device — keeps the embedder shim free of
    /// a separate `device` field on `Inner`.
    pub fn device(&self) -> &Device {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_with_missing_path_does_not_panic() {
        // Scaffold contract: `load` is a placeholder that constructs the
        // shell without touching disk. When the body lands, this test
        // becomes `expect_err` on a missing-file path.
        let cfg = ModernBertConfig::granite_r2();
        let device = Device::Cpu;
        let got = QuantizedModernBert::load(
            std::path::Path::new("/tmp/__lunaris_no_gguf_file"),
            &cfg,
            &device,
        );
        assert!(got.is_ok(), "scaffold load must not error on a missing path");
    }

    #[test]
    fn pooled_forward_is_not_yet_implemented() {
        let cfg = ModernBertConfig::granite_r2();
        let device = Device::Cpu;
        let m = QuantizedModernBert::load(
            std::path::Path::new("/tmp/__lunaris_no_gguf_file"),
            &cfg,
            &device,
        )
        .unwrap();
        let dummy_ids = Tensor::zeros((1, 4), candle_core::DType::U32, &device).unwrap();
        let dummy_mask = Tensor::ones((1, 4), candle_core::DType::U32, &device).unwrap();
        let err = m.pooled_forward(&dummy_ids, &dummy_mask).unwrap_err();
        match err {
            QuantizedError::NotImplemented(_) => {}
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }
}
