//! `CandleEmbeddingGemma` — real EmbeddingGemma 300M backed by candle.
//!
//! ## v0 forward-pass strategy
//!
//! candle 0.10.2's `candle_transformers::models::gemma3::Model::forward` returns
//! the **LM-head logits**, not the hidden states needed for sentence embeddings,
//! and `embed_tokens` is private — there is no typed `EmbeddingGemma` model in
//! candle 0.10.2 yet. v0 ships a pragmatic forward path that reads the
//! `model.embed_tokens.weight` tensor directly out of the loaded safetensors via
//! [`candle_nn::VarBuilder`], multiplies the input token ids through it, mean-pools
//! over the (non-pad) sequence axis, and L2-normalises to 768-d. This is a
//! **first-layer-only sentence embedding** — it captures lexical similarity but
//! misses contextual transformer attention. v1 swaps to a typed
//! `EmbeddingGemma` model when candle ships one (or when we promote a
//! community-contributed safetensors loader). The trait surface
//! ([`Embedder::embed_batch`]) is identical either way, so the swap is a single
//! file change with no caller impact.
//!
//! ## Latency-budget escape hatch
//!
//! If even the simplified forward path busts the per-batch budget on the dev box,
//! callers swap to `OllamaEmbedder` (or any other [`Embedder`] impl) via
//! `Lunaris::with_embedder(Arc::new(...))`. Documented in `02-01-PLAN.md`
//! <critical_constraints> and the Phase 2 CONTEXT.md "Latency-budget gamble"
//! mitigation.
//!
//! ## Failure modes
//!
//! | Condition                                         | Returned error                                                                  |
//! |---------------------------------------------------|----------------------------------------------------------------------------------|
//! | `model_path/tokenizer.json` missing               | `LunarisError::Storage(StorageError::Backend("embedding-gemma weights missing at <path> — ..."))` |
//! | `model_path/model.safetensors` missing            | same shape; message lists the safetensors path                                  |
//! | tokenizer load failure                            | `LunarisError::Storage(StorageError::Backend("embedding-gemma tokenizer: ..."))` |
//! | safetensors load failure                          | `LunarisError::Storage(StorageError::Backend("embedding-gemma weights: ..."))`   |
//! | candle tensor op failure during `embed_batch`     | `LunarisError::Storage(StorageError::Backend("candle: ..."))`                    |
//! | tokio `spawn_blocking` join failure               | `LunarisError::Storage(StorageError::Backend("candle join: ..."))`               |

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use lunaris_core::{Embedder, LunarisError, StorageError};
use tokenizers::Tokenizer;

/// Output dimensionality. EmbeddingGemma 300M is fixed at 768d (matches
/// `lunaris_core::storage::capabilities::StorageCapabilities::max_vector_dim`
/// default for the chunks/entities/facts vector indexes).
pub const EMBEDDING_GEMMA_DIM: usize = 768;

/// Maximum input tokens per request (EmbeddingGemma context window). Inputs
/// longer than this are truncated head-first to match HuggingFace tokenizer's
/// default truncation strategy.
pub const EMBEDDING_GEMMA_MAX_TOKENS: usize = 2048;

/// Construction options for [`CandleEmbeddingGemma`].
///
/// `Default` resolves `model_path` to `~/.cache/lunaris/models/embedding-gemma-300m/`
/// and `device` to `Device::Cpu` (per Plan 02-01 v0 scope; CUDA tracked separately
/// per CLAUDE.md latest-libraries policy).
#[derive(Clone, Debug)]
pub struct CandleEmbeddingGemmaOpts {
    /// Filesystem path containing `tokenizer.json` and `model.safetensors`.
    /// `None` defers to `~/.cache/lunaris/models/embedding-gemma-300m/`.
    pub model_path: Option<PathBuf>,
    /// candle compute device. v0 ships `Device::Cpu`; switch to `Device::new_cuda(0)`
    /// when a GPU is available (untested in v0 — covered by `embedder-it` in v1).
    pub device: Device,
}

impl Default for CandleEmbeddingGemmaOpts {
    fn default() -> Self {
        let cache_root = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
        let default_model_path = cache_root.join("lunaris").join("models").join("embedding-gemma-300m");
        Self { model_path: Some(default_model_path), device: Device::Cpu }
    }
}

/// Real EmbeddingGemma 300M embedder. See module-level doc for the v0 forward
/// strategy and failure-mode table.
///
/// Construction is async because tokenizer + safetensors load synchronously hit
/// the filesystem; we wrap the load in `tokio::task::spawn_blocking` to avoid
/// stalling the runtime on a cold cache.
#[derive(Clone)]
pub struct CandleEmbeddingGemma {
    /// Mean-pooled-and-L2-normalised wrapper requires the embed_tokens weight
    /// matrix; kept under `Arc` so the cheap `.clone()` at every call site
    /// shares the heap-allocated tensor.
    inner: Arc<CandleInner>,
}

impl std::fmt::Debug for CandleEmbeddingGemma {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleEmbeddingGemma")
            .field("dim", &EMBEDDING_GEMMA_DIM)
            .field("device", &format_args!("{:?}", self.inner.device))
            .field("hidden_size", &self.inner.hidden_size)
            .finish()
    }
}

struct CandleInner {
    tokenizer: Tokenizer,
    /// Token embedding matrix `[vocab_size, hidden_size=768]`. Held on the
    /// configured device so per-batch lookups are zero-copy (just a `Tensor::index_select`).
    embed_weight: Tensor,
    device: Device,
    hidden_size: usize,
}

impl CandleEmbeddingGemma {
    /// Construct a real embedder from a local model cache.
    ///
    /// Returns an actionable error if the cache directory or its required files
    /// (`tokenizer.json`, `model.safetensors`) are missing. The caller can
    /// surface that error to the user and prompt them to run
    /// `huggingface-cli download google/embeddinggemma-300m` (or equivalent).
    pub async fn new(opts: CandleEmbeddingGemmaOpts) -> Result<Self, LunarisError> {
        let model_path = opts
            .model_path
            .clone()
            .unwrap_or_else(|| CandleEmbeddingGemmaOpts::default().model_path.unwrap());

        let tokenizer_path = model_path.join("tokenizer.json");
        if !tokenizer_path.exists() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "embedding-gemma weights missing at {} — run `huggingface-cli download google/embeddinggemma-300m --local-dir {}`",
                model_path.display(),
                model_path.display()
            ))));
        }

        let safetensors_path = model_path.join("model.safetensors");
        if !safetensors_path.exists() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "embedding-gemma weights missing at {} (no model.safetensors) — run `huggingface-cli download google/embeddinggemma-300m --local-dir {}`",
                safetensors_path.display(),
                model_path.display()
            ))));
        }

        let device = opts.device.clone();
        let load = tokio::task::spawn_blocking(move || -> Result<CandleInner, LunarisError> {
            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "embedding-gemma tokenizer: {e}"
                )))
            })?;

            // CLAUDE.md forbids `unsafe`, so we use the safe buffered loader
            // (`from_buffered_safetensors`) instead of `from_mmaped_safetensors`
            // (which is `unsafe` because the underlying mapping must not be
            // mutated). Memory cost is one full safetensors-file read into the
            // process heap; for EmbeddingGemma 300M this is ~1.2 GiB at f32 —
            // acceptable for v0 single-process deployments per CLAUDE.md.
            let bytes = std::fs::read(&safetensors_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "embedding-gemma weights: read {} ({e})",
                    safetensors_path.display()
                )))
            })?;
            let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device)
                .map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "embedding-gemma weights: {e}"
                    )))
                })?;

            // The token embedding matrix lives at `model.embed_tokens.weight`
            // in HuggingFace's gemma3 safetensors layout. Shape: [vocab_size, hidden_size].
            let embed_weight = vb
                .pp("model")
                .pp("embed_tokens")
                .get_unchecked("weight")
                .map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "embedding-gemma weights: model.embed_tokens.weight not found ({e})"
                    )))
                })?;

            let dims = embed_weight.dims();
            if dims.len() != 2 {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "embedding-gemma weights: model.embed_tokens.weight has rank {} (expected 2)",
                    dims.len()
                ))));
            }
            let hidden_size = dims[1];
            if hidden_size != EMBEDDING_GEMMA_DIM {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "embedding-gemma weights: hidden_size {hidden_size} != {EMBEDDING_GEMMA_DIM}"
                ))));
            }

            Ok(CandleInner { tokenizer, embed_weight, device, hidden_size })
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("candle join: {e}")))
        })??;

        Ok(Self { inner: Arc::new(load) })
    }
}

#[async_trait]
impl Embedder for CandleEmbeddingGemma {
    fn dim(&self) -> usize {
        EMBEDDING_GEMMA_DIM
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        // Pre-tokenize on the runtime — tokenizer is fast and Send.
        let owned_inputs: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let inner = self.inner.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, LunarisError> {
            let mut out: Vec<Vec<f32>> = Vec::with_capacity(owned_inputs.len());
            for text in owned_inputs.iter() {
                let encoding = inner.tokenizer.encode(text.as_str(), true).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "embedding-gemma tokenize: {e}"
                    )))
                })?;
                let mut ids = encoding.get_ids().to_vec();
                if ids.len() > EMBEDDING_GEMMA_MAX_TOKENS {
                    ids.truncate(EMBEDDING_GEMMA_MAX_TOKENS);
                }
                if ids.is_empty() {
                    // Empty input → return a zero vector so callers don't need a
                    // pre-flight non-empty check (matches HuggingFace transformers'
                    // `mean_pooling` empty-mask behaviour).
                    out.push(vec![0.0_f32; EMBEDDING_GEMMA_DIM]);
                    continue;
                }

                // index_select: [vocab, hidden] @ [seq] → [seq, hidden]
                let id_tensor =
                    Tensor::from_vec(ids, (encoding.get_ids().len().min(EMBEDDING_GEMMA_MAX_TOKENS),), &inner.device)
                        .map_err(candle_err)?;
                let token_embeds =
                    inner.embed_weight.index_select(&id_tensor, 0).map_err(candle_err)?;

                // Mean pool over sequence axis → [hidden]
                let mean = token_embeds.mean(0).map_err(candle_err)?;

                // L2 normalise to unit length (matches sentence-transformers' default
                // post-pool norm; required for cosine similarity vector_search).
                let mean_vec: Vec<f32> = mean.to_vec1::<f32>().map_err(candle_err)?;
                let l2 = mean_vec.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
                let normalised: Vec<f32> = if l2 > f64::EPSILON {
                    mean_vec.iter().map(|x| (*x as f64 / l2) as f32).collect()
                } else {
                    mean_vec
                };
                debug_assert_eq!(normalised.len(), inner.hidden_size);
                out.push(normalised);
            }
            Ok(out)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("candle join: {e}")))
        })?
    }
}

#[inline]
fn candle_err(e: candle_core::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("candle: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_resolves_to_cache_subdir() {
        let opts = CandleEmbeddingGemmaOpts::default();
        let path = opts.model_path.expect("default sets a path");
        let s = path.to_string_lossy().to_string();
        assert!(
            s.contains("lunaris") && s.contains("models") && s.contains("embedding-gemma-300m"),
            "default model_path should include the v0 cache layout, got: {s}"
        );
    }

    #[test]
    fn dim_constant_is_768() {
        assert_eq!(EMBEDDING_GEMMA_DIM, 768);
    }
}
