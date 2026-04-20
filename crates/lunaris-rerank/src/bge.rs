//! `BgeRerankerV2M3` — bge-reranker-v2-m3 cross-encoder via candle 0.10.
//!
//! ## v0 forward-pass strategy
//!
//! candle 0.10.2 ships
//! `candle_transformers::models::xlm_roberta::XLMRobertaForSequenceClassification`
//! which is the exact architecture bge-reranker-v2-m3 uses (XLM-RoBERTa
//! backbone + classification head, num_labels=1). We load the typed model
//! from the local cache, tokenize the `(query, doc)` pair via the standard
//! XLM-RoBERTa tokenizer (CLS query SEP SEP doc SEP), forward-pass on
//! `Device::Cpu`, and take the single-label logit as the cross-encoder
//! relevance score.
//!
//! Per blueprint §4.2 latency budget the rerank pass is allocated 12 ms p50 /
//! 35 ms p99 on CPU for top-30 candidates → top-k. Cold-start model load happens
//! ONCE at construction time (wrapped in `tokio::task::spawn_blocking` so the
//! runtime never stalls), then per-call work is just tokenize + forward + sort.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` — uses the safe `VarBuilder::from_buffered_safetensors`
//!   instead of the `unsafe` mmap loader (matches the Plan 02-01 lunaris-embed pattern).
//! - `Device::Cpu` only — GPU is tracked separately per CLAUDE.md.
//! - All blocking work wrapped in `spawn_blocking` so the async runtime never stalls.
//!
//! ## Failure modes
//!
//! | Condition                                          | Returned error                                                                                                                  |
//! |----------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------|
//! | `model_path/tokenizer.json` missing                | `LunarisError::Storage(StorageError::Backend("bge-reranker-v2-m3 weights missing at <path> — ..."))`                            |
//! | `model_path/config.json` missing                   | same shape                                                                                                                       |
//! | `model_path/model.safetensors` missing             | same shape                                                                                                                       |
//! | tokenizer load failure                             | `LunarisError::Storage(StorageError::Backend("bge-reranker-v2-m3 tokenizer: ..."))`                                              |
//! | safetensors load failure                           | `LunarisError::Storage(StorageError::Backend("bge-reranker-v2-m3 weights: ..."))`                                                |
//! | candle tensor op failure during `rerank`           | `LunarisError::Storage(StorageError::Backend("bge-reranker-v2-m3 forward: ..."))`                                                |
//! | tokio `spawn_blocking` join failure                | `LunarisError::Storage(StorageError::Backend("bge-reranker-v2-m3 join: ..."))`                                                   |

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaForSequenceClassification};
use lunaris_core::{LunarisError, StorageError};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use crate::{Reranker, RerankCandidate};

/// Maximum input tokens per pair-encoding (bge-reranker-v2-m3 context window).
/// Inputs longer than this are truncated to match the model's positional limits.
pub const BGE_RERANKER_MAX_TOKENS: usize = 512;

/// Default sub-directory under the user's cache root.
const DEFAULT_CACHE_SUBDIR: &str = "lunaris/models/bge-reranker-v2-m3";

/// Construction options for [`BgeRerankerV2M3`].
///
/// `Default` resolves `model_path` to `~/.cache/lunaris/models/bge-reranker-v2-m3/`
/// and `device` to `Device::Cpu` per Plan 02-03 v0 scope.
#[derive(Clone, Debug)]
pub struct BgeRerankerV2M3Opts {
    /// Filesystem path containing `tokenizer.json`, `config.json`, and
    /// `model.safetensors`. `None` resolves to the default cache subdir.
    pub model_path: Option<PathBuf>,
    /// candle compute device. v0 ships `Device::Cpu`.
    pub device: Device,
}

impl Default for BgeRerankerV2M3Opts {
    fn default() -> Self {
        let cache_root = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
        let default_model_path = cache_root.join(DEFAULT_CACHE_SUBDIR);
        Self { model_path: Some(default_model_path), device: Device::Cpu }
    }
}

/// bge-reranker-v2-m3 cross-encoder. See module-level rustdoc for the v0 forward
/// strategy and failure-mode table.
///
/// Construction is async because tokenizer + safetensors load synchronously
/// hit the filesystem; we wrap the load in `tokio::task::spawn_blocking` to
/// avoid stalling the runtime on a cold cache.
#[derive(Clone)]
pub struct BgeRerankerV2M3 {
    inner: Arc<BgeInner>,
}

impl std::fmt::Debug for BgeRerankerV2M3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BgeRerankerV2M3")
            .field("device", &format_args!("{:?}", self.inner.device))
            .field("max_tokens", &BGE_RERANKER_MAX_TOKENS)
            .finish()
    }
}

struct BgeInner {
    tokenizer: Tokenizer,
    model: XLMRobertaForSequenceClassification,
    device: Device,
}

impl BgeRerankerV2M3 {
    /// Construct a real reranker from the default `~/.cache/lunaris/models/bge-reranker-v2-m3/`
    /// directory.
    ///
    /// On cache miss returns the actionable error
    /// `bge-reranker-v2-m3 weights missing at <path> — download via huggingface-cli ...`.
    /// The umbrella `Lunaris::open(url)` catches this and substitutes
    /// [`crate::NoopReranker`] per the RETRIEVE-06 contract.
    pub async fn try_new_from_default_cache() -> Result<Self, LunarisError> {
        Self::try_new(BgeRerankerV2M3Opts::default()).await
    }

    /// Construct from an explicit model path (escape hatch for tests + non-default
    /// cache layouts).
    pub async fn try_new_from_path(model_path: PathBuf) -> Result<Self, LunarisError> {
        Self::try_new(BgeRerankerV2M3Opts { model_path: Some(model_path), device: Device::Cpu })
            .await
    }

    /// Construct with full options.
    pub async fn try_new(opts: BgeRerankerV2M3Opts) -> Result<Self, LunarisError> {
        let model_path = opts
            .model_path
            .clone()
            .unwrap_or_else(|| BgeRerankerV2M3Opts::default().model_path.unwrap());

        // Validate required files exist BEFORE spawning the blocking load —
        // gives a fast actionable error on cache miss without burning a
        // task slot.
        let tokenizer_path = model_path.join("tokenizer.json");
        let config_path = model_path.join("config.json");
        let weights_path = model_path.join("model.safetensors");
        for (label, p) in [
            ("tokenizer.json", &tokenizer_path),
            ("config.json", &config_path),
            ("model.safetensors", &weights_path),
        ] {
            if !p.exists() {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 weights missing at {} (no {label}) — download via `python -m huggingface_hub download BAAI/bge-reranker-v2-m3 --local-dir {}`",
                    model_path.display(),
                    model_path.display()
                ))));
            }
        }

        let device = opts.device.clone();
        let load = tokio::task::spawn_blocking(move || -> Result<BgeInner, LunarisError> {
            // 1. Tokenizer.
            let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 tokenizer: {e}"
                )))
            })?;
            // Truncate to the model's positional limit; pad to longest in batch
            // so a single forward pass scores all pairs at once.
            let _ = tokenizer.with_truncation(Some(TruncationParams {
                max_length: BGE_RERANKER_MAX_TOKENS,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                direction: tokenizers::TruncationDirection::Right,
            }));
            tokenizer.with_padding(Some(PaddingParams {
                strategy: PaddingStrategy::BatchLongest,
                direction: tokenizers::PaddingDirection::Right,
                pad_id: 1, // XLM-R pad id
                pad_type_id: 0,
                pad_token: "<pad>".to_string(),
                pad_to_multiple_of: None,
            }));

            // 2. Config (parsed via the typed candle config).
            let cfg_bytes = std::fs::read(&config_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 config: read {} ({e})",
                    config_path.display()
                )))
            })?;
            let cfg: Config = serde_json::from_slice(&cfg_bytes).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 config: parse {} ({e})",
                    config_path.display()
                )))
            })?;

            // 3. Safetensors (safe buffered loader; CLAUDE.md no-unsafe).
            let bytes = std::fs::read(&weights_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 weights: read {} ({e})",
                    weights_path.display()
                )))
            })?;
            let vb =
                VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "bge-reranker-v2-m3 weights: {e}"
                    )))
                })?;

            // bge-reranker-v2-m3 has num_labels=1 (single relevance logit).
            let model = XLMRobertaForSequenceClassification::new(1, &cfg, vb).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 model construct: {e}"
                )))
            })?;

            Ok(BgeInner { tokenizer, model, device })
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("bge-reranker-v2-m3 join: {e}")))
        })??;

        Ok(Self { inner: Arc::new(load) })
    }
}

#[async_trait]
impl Reranker for BgeRerankerV2M3 {
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

        let inner = self.inner.clone();
        let owned_query = query.to_string();

        tokio::task::spawn_blocking(move || -> Result<Vec<RerankCandidate>, LunarisError> {
            // Build (query, doc) pair encodings. The tokenizer's `encode_batch`
            // with the pre-configured padding/truncation produces a uniform
            // [batch, max_seq] tensor in one call — defends against
            // T-02-03-02 (adversarial chunk text) by treating doc.text as
            // opaque bytes per HuggingFace tokenizers contract.
            let pairs: Vec<(String, String)> =
                docs.iter().map(|d| (owned_query.clone(), d.text.clone())).collect();
            let encodings = inner.tokenizer.encode_batch(pairs, true).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "bge-reranker-v2-m3 tokenize: {e}"
                )))
            })?;

            let batch_size = encodings.len();
            let max_seq = encodings.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
            if max_seq == 0 {
                // Edge case: every input encoded to zero tokens (shouldn't
                // happen with non-empty docs but guard anyway).
                let mut out = docs;
                // Drop scores to 0 — caller's downstream sort is stable.
                for d in &mut out {
                    d.score = 0.0;
                }
                return Ok(out);
            }

            // Stack input_ids / attention_mask / token_type_ids into [batch, seq] tensors.
            let mut all_ids: Vec<u32> = Vec::with_capacity(batch_size * max_seq);
            let mut all_mask: Vec<u32> = Vec::with_capacity(batch_size * max_seq);
            let mut all_types: Vec<u32> = Vec::with_capacity(batch_size * max_seq);
            for enc in &encodings {
                all_ids.extend_from_slice(enc.get_ids());
                all_mask.extend_from_slice(enc.get_attention_mask());
                all_types.extend_from_slice(enc.get_type_ids());
            }

            let input_ids = Tensor::from_vec(all_ids, (batch_size, max_seq), &inner.device)
                .map_err(forward_err)?;
            let attention_mask =
                Tensor::from_vec(all_mask, (batch_size, max_seq), &inner.device)
                    .map_err(forward_err)?;
            let token_type_ids =
                Tensor::from_vec(all_types, (batch_size, max_seq), &inner.device)
                    .map_err(forward_err)?;

            // Forward → logits shape [batch, 1] (num_labels=1 for bge-reranker-v2-m3).
            let logits =
                inner.model.forward(&input_ids, &attention_mask, &token_type_ids).map_err(forward_err)?;

            // Squeeze to [batch] then collect to a Vec<f32> for downstream sort.
            let scores: Vec<f32> = logits
                .squeeze(1)
                .map_err(forward_err)?
                .to_vec1::<f32>()
                .map_err(forward_err)?;

            // Pair scores with docs, sort desc by score, return.
            let mut scored: Vec<(f32, RerankCandidate)> = scores
                .into_iter()
                .zip(docs)
                .map(|(s, mut d)| {
                    d.score = s;
                    (s, d)
                })
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            Ok(scored.into_iter().map(|(_, d)| d).collect())
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("bge-reranker-v2-m3 join: {e}")))
        })?
    }
}

#[inline]
fn forward_err(e: candle_core::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("bge-reranker-v2-m3 forward: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_resolves_to_cache_subdir() {
        let opts = BgeRerankerV2M3Opts::default();
        let path = opts.model_path.expect("default sets a path");
        let s = path.to_string_lossy().to_string();
        assert!(
            s.contains("lunaris") && s.contains("models") && s.contains("bge-reranker-v2-m3"),
            "default model_path should include the v0 cache layout, got: {s}"
        );
    }

    #[test]
    fn max_tokens_constant_is_512() {
        assert_eq!(BGE_RERANKER_MAX_TOKENS, 512);
    }
}
