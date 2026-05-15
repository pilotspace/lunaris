//! `NativeReranker` — `lunaris_rerank::Reranker` impl backed by candle's
//! XLM-RoBERTa cross-encoder for `BAAI/bge-reranker-v2-m3`.
//!
//! See [`crate`] for the architectural rationale. This module:
//! - holds the `Arc<Inner>` cheap-clone handle,
//! - constructs the model from safetensors + `tokenizer.json` + `config.json`,
//! - implements `Reranker::rerank` via `tokio::task::spawn_blocking` — no
//!   lock held across `.await`,
//! - sorts the returned `RerankCandidate`s by score descending.
//!
//! ## Failure modes
//!
//! | Condition                              | Returned error                                       |
//! |----------------------------------------|------------------------------------------------------|
//! | safetensors file missing / unreadable  | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | tokenizer.json missing / unreadable    | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | config.json missing / invalid          | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | candle forward pass failure            | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | spawn_blocking join failure (panic)    | `LunarisError::Storage(StorageError::Backend(..))`   |

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use lunaris_core::{LunarisError, StorageError};
use lunaris_rerank::{RerankCandidate, Reranker};

use crate::config::{ConfigError, XlmRobertaRerankerConfig};
use crate::tokenizer::{EncodedPairBatch, PairTokenizer, TokenizerError};
use crate::xlmr_reranker::{ForwardError, XlmRobertaReranker};

/// Public construction options.
///
/// All three paths are required; defaults are not provided because there's no
/// canonical on-disk layout for bge-reranker-v2-m3 (it lives wherever the
/// operator downloaded it). Tests use the env-var convention
/// `BGE_RERANKER_WEIGHTS_PATH` / `BGE_RERANKER_TOKENIZER_PATH` /
/// `BGE_RERANKER_CONFIG_PATH` to point at the local cache.
#[derive(Clone, Debug)]
pub struct NativeRerankerOpts {
    pub weights_path: PathBuf,
    pub tokenizer_path: PathBuf,
    pub config_path: PathBuf,
    pub device: Device,
}

/// Errors raised during construction. Hot-path errors are mapped to
/// `LunarisError` via the trait surface; this enum exists so callers can
/// inspect / log specific variants before falling back.
#[derive(Debug, thiserror::Error)]
pub enum NativeRerankerError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    #[error("tokenizer: {0}")]
    Tokenizer(#[from] TokenizerError),

    #[error("weights: {0}")]
    Weights(String),

    #[error("forward: {0}")]
    Forward(#[from] ForwardError),

    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),
}

impl From<NativeRerankerError> for LunarisError {
    fn from(e: NativeRerankerError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!("lunaris-rerank-native: {e}")))
    }
}

/// bge-reranker-v2-m3 native reranker. Cheap to clone — the heavy state lives
/// behind an `Arc<Inner>`.
#[derive(Clone)]
pub struct NativeReranker {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NativeReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeReranker").field("max_len", &self.inner.tokenizer.max_len()).finish()
    }
}

struct Inner {
    /// The cross-encoder forward-pass model. candle's
    /// `XLMRobertaForSequenceClassification::forward` takes `&self`; concurrent
    /// `rerank` calls run truly in parallel (each on a `spawn_blocking`
    /// thread). CLAUDE.md lock-across-await constraint is satisfied vacuously:
    /// no lock is acquired on the hot path.
    model: XlmRobertaReranker,
    tokenizer: PairTokenizer,
    device: Device,
}

impl NativeReranker {
    /// Construct from on-disk artifacts. Synchronous I/O happens inline
    /// (reads safetensors, parses tokenizer, loads weights into the chosen
    /// `Device`). Callers concerned about runtime stalls should wrap in
    /// `tokio::task::spawn_blocking`; the load path itself is not async
    /// because error mapping stays straightforward this way.
    pub fn open(opts: NativeRerankerOpts) -> Result<Self, NativeRerankerError> {
        // O-01-B — physical-core rayon pool init. Idempotent with the
        // embedder side's call (shared global). No-op on Metal/CUDA.
        crate::rayon_pool::ensure_physical_core_pool();

        // O-01-C/D — Device upgrade.
        let mut opts = opts;
        opts.device = crate::device_select::select_device(opts.device);

        let cfg = XlmRobertaRerankerConfig::try_from_json_path(&opts.config_path)?;
        let tokenizer = PairTokenizer::from_file(&opts.tokenizer_path, cfg.pad_token_id)?;

        tracing::info!(
            backend = "lunaris-rerank-native",
            model = "bge-reranker-v2-m3",
            weights = %opts.weights_path.display(),
            "native reranker loading"
        );

        // Compute in FP32. Match the embedder side's rationale: the
        // classification head's tanh + sigmoid are sensitive enough that
        // fp16 leaks measurable drift on the cross-encoder calibration. The
        // Q4_K_M follow-up (N-02 step 2) re-evaluates this trade-off.
        let vb = candle_safetensors_varbuilder(&opts.weights_path, &opts.device)?;
        let model = XlmRobertaReranker::load(vb, &cfg)?;

        tracing::info!(
            backend = "lunaris-rerank-native",
            model = "bge-reranker-v2-m3",
            "native reranker initialized"
        );

        // O-01-C — warm-up matmul on the selected device.
        crate::device_select::warmup_device(&opts.device);

        Ok(Self { inner: Arc::new(Inner { model, tokenizer, device: opts.device }) })
    }

    /// Synchronous score path — for tests / direct callers that already have
    /// the inputs marshalled. The async trait method wraps this in
    /// `spawn_blocking`.
    ///
    /// Returns scores in input order (NOT sorted) — the public `rerank`
    /// method sorts after pairing scores with their candidates.
    pub fn score_blocking(&self, pairs: &[(&str, &str)]) -> Result<Vec<f32>, NativeRerankerError> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let EncodedPairBatch { input_ids, attention_mask, token_type_ids } =
            self.inner.tokenizer.encode_pair_batch(pairs, &self.inner.device)?;
        let scores = self.inner.model.score(&input_ids, &attention_mask, &token_type_ids)?;
        Ok(scores.to_vec1::<f32>()?)
    }
}

/// Build a VarBuilder reading safetensors from disk in FP32 via a buffered
/// (non-mmap) loader. CLAUDE.md `#![forbid(unsafe_code)]` rules out
/// `VarBuilder::from_mmaped_safetensors` (which is `unsafe fn`); the
/// `from_buffered_safetensors` path is safe — it reads bytes into a `Vec` up
/// front, then candle decodes tensors lazily as `VarBuilder::get` is called.
///
/// Trade-off: ~1.1 GB transient RSS during load. Acceptable for the v0.4
/// milestone; the FP32 compute dtype means steady-state RSS is ~2.3 GB
/// regardless. The Q4 follow-up reconsiders both knobs together.
fn candle_safetensors_varbuilder(
    path: &std::path::Path,
    device: &Device,
) -> Result<VarBuilder<'static>, NativeRerankerError> {
    let bytes = std::fs::read(path).map_err(|e| {
        NativeRerankerError::Weights(format!(
            "safetensors read from {} failed: {e}",
            path.display()
        ))
    })?;
    VarBuilder::from_buffered_safetensors(bytes, DType::F32, device).map_err(|e| {
        NativeRerankerError::Weights(format!(
            "safetensors decode from {} failed: {e}",
            path.display()
        ))
    })
}

#[async_trait]
impl Reranker for NativeReranker {
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

        // Move owned inputs across the blocking boundary; `&str` borrows are
        // not `'static`.
        let owned_query = query.to_string();
        let me = self.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<RerankCandidate>, LunarisError> {
            // Build `(query, doc.text)` pairs as borrows over the owned
            // RerankCandidate vector — saves one clone of the doc text per
            // pair (only the query is cloned, once).
            let pairs: Vec<(&str, &str)> =
                docs.iter().map(|d| (owned_query.as_str(), d.text.as_str())).collect();
            let scores = me.score_blocking(&pairs).map_err(LunarisError::from)?;

            // Pair scores with docs, mutate score in place, sort desc.
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
            LunarisError::Storage(StorageError::Backend(format!("lunaris-rerank-native join: {e}")))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trait dyn-compat: `NativeReranker` MUST be usable as `Arc<dyn Reranker>`
    // (the public Lunaris construction pattern).
    #[allow(dead_code)]
    fn _dyn_compat(r: NativeReranker) -> std::sync::Arc<dyn Reranker> {
        std::sync::Arc::new(r)
    }

    #[test]
    fn open_with_missing_config_fails_fast() {
        let opts = NativeRerankerOpts {
            weights_path: PathBuf::from("/tmp/__lunaris_no_weights.safetensors"),
            tokenizer_path: PathBuf::from("/tmp/__lunaris_no_tokenizer.json"),
            config_path: PathBuf::from("/tmp/__lunaris_no_config.json"),
            device: Device::Cpu,
        };
        let err = NativeReranker::open(opts).expect_err("missing files must fail");
        match err {
            NativeRerankerError::Config(_) => {}
            other => panic!("expected Config error, got: {other:?}"),
        }
    }
}
