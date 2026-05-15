//! `NativeQuantizedReranker` — `lunaris_rerank::Reranker` impl backed by
//! Q4_K_M GGUF `bge-reranker-v2-m3` (scaffold).
//!
//! Mirrors the structure of `crate::reranker::NativeReranker`:
//! - `Arc<Inner>` cheap-clone handle
//! - tokenizer + model + device live inside `Inner`
//! - `Reranker::rerank` defers to `tokio::task::spawn_blocking`; no lock is
//!   held across `.await` (vacuous — no lock at all on the hot path; the
//!   model state is read-only after load).
//!
//! **Scaffold status (2026-05-14):** `open` returns `NotImplemented` because
//! `QuantizedXlmRoberta::load` is a placeholder. The RED integration test
//! (`tests/quantized_equivalence.rs`) exists and will fail until the next
//! commit lands the forward pass — that's the point of RED-first TDD per
//! CLAUDE.md Rule 3.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::Device;
use lunaris_core::{LunarisError, StorageError};
use lunaris_rerank::{RerankCandidate, Reranker};

use crate::config::{ConfigError, XlmRobertaRerankerConfig};
use crate::quantized_xlmr::{QuantizedRerankError, QuantizedXlmRoberta};
use crate::tokenizer::{EncodedPairBatch, PairTokenizer, TokenizerError};

/// Construction options for the quantized path. Mirrors
/// `NativeRerankerOpts` but reads a `.gguf` file instead of safetensors.
///
/// The `config_path` is still required and refers to the SAME `config.json`
/// the FP32 path consumes — the GGUF metadata is not trusted as the source
/// of truth for hyperparameters (mirrors the embedder Q4 path's lesson on
/// `pooling_type`).
#[derive(Clone, Debug)]
pub struct NativeQuantizedRerankerOpts {
    /// Path to the `bge-reranker-v2-m3-Q4_K_M.gguf` produced by
    /// `scripts/spike-convert-bge-reranker-to-gguf.sh`. SHA-256 is pinned in
    /// `crate::BGE_RERANKER_GGUF_Q4_SHA256`.
    pub gguf_path: PathBuf,
    /// Path to the HF `tokenizer.json` (identical to the FP32 path's input).
    pub tokenizer_path: PathBuf,
    /// Path to the HF `config.json` (identical to the FP32 path's input).
    pub config_path: PathBuf,
    /// candle device. `Device::Cpu` is the only verified target until the
    /// hardware-optimization milestone wires Metal/CUDA.
    pub device: Device,
}

/// Errors raised during construction or the hot path.
#[derive(Debug, thiserror::Error)]
pub enum NativeQuantizedRerankerError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    #[error("tokenizer: {0}")]
    Tokenizer(#[from] TokenizerError),

    #[error("quantized: {0}")]
    Quantized(#[from] QuantizedRerankError),

    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),
}

impl From<NativeQuantizedRerankerError> for LunarisError {
    fn from(e: NativeQuantizedRerankerError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!(
            "lunaris-rerank-native (quantized): {e}"
        )))
    }
}

/// Quantized bge-reranker-v2-m3 reranker. Cheap to clone — the heavy state
/// lives behind an `Arc<Inner>`.
#[derive(Clone)]
pub struct NativeQuantizedReranker {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NativeQuantizedReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeQuantizedReranker")
            .field("max_len", &self.inner.tokenizer.max_len())
            .field("sha256", &crate::BGE_RERANKER_GGUF_Q4_SHA256)
            .finish()
    }
}

struct Inner {
    model: QuantizedXlmRoberta,
    tokenizer: PairTokenizer,
    device: Device,
}

impl NativeQuantizedReranker {
    /// Construct from on-disk artifacts. Synchronous I/O for the same reasons
    /// the FP32 path takes — error mapping stays straightforward and callers
    /// concerned about runtime stalls can `spawn_blocking` at their own
    /// boundary.
    pub fn open(opts: NativeQuantizedRerankerOpts) -> Result<Self, NativeQuantizedRerankerError> {
        // O-01-B — physical-core rayon pool init.
        crate::rayon_pool::ensure_physical_core_pool();

        // O-01-C/D — Device upgrade.
        let mut opts = opts;
        opts.device = crate::device_select::select_device(opts.device);

        let cfg = XlmRobertaRerankerConfig::try_from_json_path(&opts.config_path)?;
        let tokenizer = PairTokenizer::from_file(&opts.tokenizer_path, cfg.pad_token_id)?;
        let model = QuantizedXlmRoberta::load(&opts.gguf_path, &cfg, &opts.device)?;

        tracing::info!(
            backend = "lunaris-rerank-native (quantized)",
            model = "bge-reranker-v2-m3",
            gguf = %opts.gguf_path.display(),
            sha256 = %crate::BGE_RERANKER_GGUF_Q4_SHA256,
            "quantized reranker loaded"
        );

        // O-01-C — warm-up matmul.
        crate::device_select::warmup_device(&opts.device);

        Ok(Self { inner: Arc::new(Inner { model, tokenizer, device: opts.device }) })
    }

    /// Diagnostic-only accessor for the inner `QuantizedXlmRoberta`. Used by
    /// the layerwise-diff test to call `forward_hidden`. Not part of the
    /// stable public API; gated to test-relevant work only.
    #[doc(hidden)]
    pub fn inner_for_test(&self) -> &QuantizedXlmRoberta {
        &self.inner.model
    }

    /// Synchronous score path — for tests / direct callers that already have
    /// the inputs marshalled. The async trait method wraps this in
    /// `spawn_blocking`.
    ///
    /// Returns scores in input order (NOT sorted) — the public `rerank`
    /// method sorts after pairing scores with their candidates.
    pub fn score_blocking(
        &self,
        pairs: &[(&str, &str)],
    ) -> Result<Vec<f32>, NativeQuantizedRerankerError> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let EncodedPairBatch { input_ids, attention_mask, token_type_ids } =
            self.inner.tokenizer.encode_pair_batch(pairs, &self.inner.device)?;
        let scores = self.inner.model.score(&input_ids, &attention_mask, &token_type_ids)?;
        Ok(scores.to_vec1::<f32>()?)
    }
}

#[async_trait]
impl Reranker for NativeQuantizedReranker {
    fn applies(&self) -> bool {
        true
    }

    async fn rerank(
        &self,
        query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }

        let owned_query = query.to_string();
        let me = self.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<RerankCandidate>, LunarisError> {
            let pairs: Vec<(&str, &str)> =
                docs.iter().map(|d| (owned_query.as_str(), d.text.as_str())).collect();
            let scores = me.score_blocking(&pairs).map_err(LunarisError::from)?;

            let mut scored: Vec<(f32, RerankCandidate)> = scores
                .into_iter()
                .zip(docs)
                .map(|(s, mut d)| {
                    d.score = s;
                    (s, d)
                })
                .collect();
            // NaN sentinel: bubble NaNs to the bottom so an isolated forward-pass
            // hiccup doesn't poison the top-k slot. partial_cmp returns None on
            // NaN; treat that as "less than" the other side.
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            Ok(scored.into_iter().map(|(_, d)| d).collect())
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-rerank-native (quantized) join: {e}"
            )))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn _dyn_compat(r: NativeQuantizedReranker) -> std::sync::Arc<dyn Reranker> {
        std::sync::Arc::new(r)
    }

    #[test]
    fn open_with_missing_config_fails_fast() {
        let opts = NativeQuantizedRerankerOpts {
            gguf_path: PathBuf::from("/tmp/__lunaris_no_rerank_gguf.gguf"),
            tokenizer_path: PathBuf::from("/tmp/__lunaris_no_rerank_tokenizer.json"),
            config_path: PathBuf::from("/tmp/__lunaris_no_rerank_config.json"),
            device: Device::Cpu,
        };
        let err = NativeQuantizedReranker::open(opts).expect_err("missing files must fail");
        match err {
            NativeQuantizedRerankerError::Config(_) => {}
            other => panic!("expected Config error, got: {other:?}"),
        }
    }
}
