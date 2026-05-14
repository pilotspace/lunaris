//! `quantized_xlmr` — Q4_K_M XLM-RoBERTa forward pass for `bge-reranker-v2-m3`.
//!
//! **Scaffold status (2026-05-14):** this module wires up the public surface
//! (`QuantizedXlmRoberta::load` + `score`) so the `quantized_reranker` crate
//! can compile under `--features reranker-gguf`. The forward-pass body lands
//! in the next commit (drift-gate-driven). The RED equivalence test gates
//! delivery — see `tests/quantized_equivalence.rs`.
//!
//! ## Numerical-equivalence contract (target)
//!
//! Mirrors the FP32 reference (`candle_transformers::models::xlm_roberta`).
//! Linear projections become `QMatMul`; LayerNorms stay F32 (norm scales are
//! stored as F32 in the GGUF). Forward math is otherwise byte-for-byte the
//! same: 24 layers of all-global attention, learned absolute position
//! embeddings, GELU MLP, CLS pool, classification head dense(1024→1024) →
//! tanh → out_proj(1024→1) → sigmoid.
//!
//! ## XLM-RoBERTa vs ModernBERT (granite-r2)
//!
//! | Aspect            | granite-r2 (N-01.5)       | bge-reranker-v2-m3 (this)   |
//! |-------------------|----------------------------|------------------------------|
//! | Position embed    | RoPE per-layer             | Learned absolute             |
//! | Attention scope   | Alternating local/global   | All global                   |
//! | MLP               | GeGLU (fused gate+up)      | GELU (separate up→down)      |
//! | Norm              | LayerNorm no-bias          | LayerNorm WITH bias          |
//! | Type embed        | none                       | Yes, single row              |
//! | Head              | none (pooled output)       | dense + tanh + out_proj      |
//!
//! ## Classifier head fate (decided at manifest dump time)
//!
//! `convert_hf_to_gguf.py` is BERT-family-aware but its handling of the
//! XLM-R `XLMRobertaForSequenceClassification` head is uncertain. The
//! conversion-evidence doc records which case fired:
//!
//! - **Case A**: classifier tensors are in the GGUF (`cls.weight` / `cls.bias`
//!   or `classifier.dense.weight` / `classifier.out_proj.weight` — names vary
//!   by converter version). We load via `QMatMul`/dequant in this module.
//! - **Case B**: classifier tensors are NOT in the GGUF. The brief
//!   explicitly permits a fallback: load the head from the original
//!   `model.safetensors` as F32 at construction time. This adds ~8 MB to
//!   RSS (`1024² × 4` bytes for dense, `1024 × 4` for out_proj) and a F32
//!   path on the final 1024-dim CLS vector only — well below the
//!   `≤60% of FP32 RSS` gate.

#![allow(dead_code)] // scaffold — fields wired in next commit

use std::path::Path;

use candle_core::Device;

use crate::config::XlmRobertaRerankerConfig;

/// Errors raised by the quantized path. Mirrors `ForwardError` so callers can
/// do uniform error mapping into `NativeQuantizedRerankerError`.
#[derive(Debug, thiserror::Error)]
pub enum QuantizedRerankError {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("gguf: {reason}")]
    Gguf { reason: String },

    #[error("forward: model returned shape {actual:?}, expected (batch, {expected})")]
    UnexpectedShape { actual: Vec<usize>, expected: usize },

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

/// Q4_K_M XLM-RoBERTa cross-encoder loaded from a GGUF file. Forward pass
/// (`score`) returns sigmoid-bounded scores in `[0, 1]`, matching the FP32
/// path's output contract.
pub struct QuantizedXlmRoberta {
    cfg: XlmRobertaRerankerConfig,
    device: Device,
}

impl QuantizedXlmRoberta {
    /// Load the model from a GGUF file. `cfg` MUST already be validated
    /// (use `XlmRobertaRerankerConfig::try_from_json_path`) — we do NOT
    /// re-parse the config from GGUF metadata (mirrors the embedder Q4
    /// path's lesson that GGUF `pooling_type` is misleading).
    pub fn load(
        _path: &Path,
        cfg: &XlmRobertaRerankerConfig,
        device: &Device,
    ) -> Result<Self, QuantizedRerankError> {
        // Scaffold: defer to the next commit. The struct must still be
        // constructible so `quantized_reranker.rs` can compile; load() will
        // fail with NotImplemented until the forward pass lands. That keeps
        // the RED equivalence test failing for a clear reason.
        Err(QuantizedRerankError::NotImplemented(
            "QuantizedXlmRoberta::load — forward pass body lands in next commit \
             (drift-gate-driven). cfg/device captured for signature only.",
        ))?;
        // unreachable but keeps the field-init analysis honest while the
        // body is in scaffold mode.
        #[allow(unreachable_code)]
        Ok(Self { cfg: cfg.clone(), device: device.clone() })
    }

    /// Synchronous score path. `input_ids`, `attention_mask`, `token_type_ids`:
    /// shape `(batch, seq)`, `DType::U32`. Returns `(batch,)` F32 sigmoid
    /// scores in `[0, 1]`.
    pub fn score(
        &self,
        _input_ids: &candle_core::Tensor,
        _attention_mask: &candle_core::Tensor,
        _token_type_ids: &candle_core::Tensor,
    ) -> Result<candle_core::Tensor, QuantizedRerankError> {
        Err(QuantizedRerankError::NotImplemented(
            "QuantizedXlmRoberta::score — forward pass body lands in next commit",
        ))
    }

    /// Accessor for the loaded config.
    pub fn config(&self) -> &XlmRobertaRerankerConfig {
        &self.cfg
    }

    /// Accessor for the model's device.
    pub fn device(&self) -> &Device {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_with_missing_path_errors() {
        let cfg = XlmRobertaRerankerConfig::bge_reranker_v2_m3();
        let device = Device::Cpu;
        let got = QuantizedXlmRoberta::load(
            std::path::Path::new("/tmp/__lunaris_no_rerank_gguf_file"),
            &cfg,
            &device,
        );
        assert!(got.is_err(), "load must fail on missing gguf");
    }
}
