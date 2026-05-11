//! [`CandleGemma3_270M`] — Gemma-3 270M IT verifier backed by candle 0.10's
//! typed `candle_transformers::models::gemma3::Model`.
//!
//! ## Wiring strategy (RFC 0006)
//!
//! Scaffold for the v0.2.x verifier default swap. Mirrors
//! `candle_gemma3_27b.rs` byte-for-byte where possible — same constructor
//! shape (`new`, `try_new_from_default_cache`, `try_new_from_path`), same
//! `Verifier` trait impl, same `spawn_blocking` + `parking_lot::Mutex`
//! discipline. The divergences are constants:
//!
//! - Model weights path: `~/.cache/lunaris/models/gemma-3-270m-it/` (NOT
//!   `gemma-3-27b-it`). The cache-miss error message carries the actionable
//!   `huggingface-cli download google/gemma-3-270m-it` hint plus a corrected
//!   disk + RAM headroom note (~600 MB disk / ~1 GB RAM — vs the 27B's
//!   ~16 GB / ~24 GB).
//! - Per-batch timeout dropped 7x to 200 ms (270M forward pass on CPU is
//!   ~5-10x faster than 4B, ~50x faster than 27B).
//! - `DEFAULT_PER_CHUNK_TIMEOUT_MS = 100`.
//! - `DEFAULT_MAX_NEW_TOKENS = 512` — arbitration decisions need fewer
//!   tokens at 270M (binary winner/loser + 1-sentence reason).
//!
//! ## RFC 0006 status (Draft)
//!
//! This file lands as the *scaffold* behind `--features verify-small`. The
//! production default flip (Verifier 27B → 270M) is gated on the Phase 24
//! head-to-head bench numbers + the 100-item quality gate. Until that
//! gate passes, 270M ships as opt-in. See `docs/rfcs/0006-verifier-default-swap.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::gemma3::{Config as Gemma3Config, Model as Gemma3Model};
use lunaris_core::{LunarisError, StorageError};
use parking_lot::Mutex;
use tokenizers::Tokenizer;

use crate::Verifier;
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

/// Default sub-directory under the user's cache root.
const DEFAULT_CACHE_SUBDIR: &str = "lunaris/models/gemma-3-270m-it";

/// Default per-batch timeout — 270M is ~50x faster than 27B on CPU and
/// ~5-10x faster than 4B. The 200 ms budget gives generous headroom on
/// modest hardware (2-core CPU, 2 GB RAM laptop floor — RFC 0006 §3).
pub const DEFAULT_BATCH_TIMEOUT_MS: u64 = 200;

/// Default per-chunk fallback timeout. Mirrors the batch ratio in the
/// 27B file (per_chunk ≈ batch / 2).
pub const DEFAULT_PER_CHUNK_TIMEOUT_MS: u64 = 100;

/// Default max-new-tokens cap. Arbitration decisions at 270M are
/// shorter than at 27B — the model emits the winner-id + a 1-sentence
/// reason. Bumped only if Phase 24 bench shows quality degradation
/// attributable to truncation.
pub const DEFAULT_MAX_NEW_TOKENS: usize = 512;

/// Construction options for [`CandleGemma3_270M`].
///
/// `Default` resolves `model_path` to
/// `~/.cache/lunaris/models/gemma-3-270m-it/`, `device` to `Device::Cpu`,
/// `batch_timeout_ms` to [`DEFAULT_BATCH_TIMEOUT_MS`], `per_chunk_timeout_ms`
/// to [`DEFAULT_PER_CHUNK_TIMEOUT_MS`], and `max_new_tokens` to
/// [`DEFAULT_MAX_NEW_TOKENS`].
#[derive(Clone, Debug)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_270MOpts {
    /// Filesystem path containing `tokenizer.json`, `config.json`, and
    /// `model.safetensors`. `None` resolves to the default cache subdir.
    pub model_path: Option<PathBuf>,
    /// candle compute device. v0 ships `Device::Cpu`.
    pub device: Device,
    /// Per-batch timeout.
    pub batch_timeout_ms: u64,
    /// Per-chunk timeout for the per-chunk fallback path.
    pub per_chunk_timeout_ms: u64,
    /// Max tokens emitted per verify call's decode loop.
    pub max_new_tokens: usize,
}

impl Default for CandleGemma3_270MOpts {
    fn default() -> Self {
        let cache_root = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            model_path: Some(cache_root.join(DEFAULT_CACHE_SUBDIR)),
            device: Device::Cpu,
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            per_chunk_timeout_ms: DEFAULT_PER_CHUNK_TIMEOUT_MS,
            max_new_tokens: DEFAULT_MAX_NEW_TOKENS,
        }
    }
}

/// Real Gemma-3 270M verifier. Construction shape mirrors
/// `CandleGemma3_27B::new` so the dispatch site in `default_verifier`
/// can swap one for the other behind a feature flag without other code
/// changes.
#[derive(Clone)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_270M {
    inner: Arc<CandleInner>,
}

impl std::fmt::Debug for CandleGemma3_270M {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleGemma3_270M")
            .field("device", &format_args!("{:?}", self.inner.device))
            .field("max_new_tokens", &self.inner.max_new_tokens)
            .field("batch_timeout_ms", &self.inner.batch_timeout_ms)
            .field("per_chunk_timeout_ms", &self.inner.per_chunk_timeout_ms)
            .finish()
    }
}

struct CandleInner {
    tokenizer: Tokenizer,
    model: Mutex<Gemma3Model>,
    device: Device,
    batch_timeout_ms: u64,
    per_chunk_timeout_ms: u64,
    max_new_tokens: usize,
}

impl CandleGemma3_270M {
    /// Construct from the default cache.
    pub async fn try_new_from_default_cache() -> Result<Self, LunarisError> {
        Self::new(CandleGemma3_270MOpts::default()).await
    }

    /// Construct from an explicit model path (escape hatch for tests + non-
    /// default cache layouts). Other options take their defaults.
    pub async fn try_new_from_path(model_path: PathBuf) -> Result<Self, LunarisError> {
        Self::new(CandleGemma3_270MOpts {
            model_path: Some(model_path),
            ..CandleGemma3_270MOpts::default()
        })
        .await
    }

    /// Construct with full options. Returns the actionable cache-miss error
    /// ```text
    /// gemma-3-270m-it weights missing at PATH — run `huggingface-cli download google/gemma-3-270m-it --local-dir PATH` (note: 270M requires ~600MB disk + ~1GB RAM for inference)
    /// ```
    /// when `tokenizer.json`, `config.json`, or `model.safetensors` are
    /// missing — `default_verifier` catches this and substitutes
    /// [`crate::NoopVerifier`] with `tracing::warn!`.
    pub async fn new(opts: CandleGemma3_270MOpts) -> Result<Self, LunarisError> {
        let model_path = opts
            .model_path
            .clone()
            .unwrap_or_else(|| CandleGemma3_270MOpts::default().model_path.unwrap());

        let tokenizer_path = model_path.join("tokenizer.json");
        let config_path = model_path.join("config.json");
        let safetensors_path = model_path.join("model.safetensors");
        for (label, p) in [
            ("tokenizer.json", &tokenizer_path),
            ("config.json", &config_path),
            ("model.safetensors", &safetensors_path),
        ] {
            if !p.exists() {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-270m-it weights missing at {} (no {label}) — run `huggingface-cli download google/gemma-3-270m-it --local-dir {}` (note: 270M requires ~600MB disk + ~1GB RAM for inference)",
                    model_path.display(),
                    model_path.display()
                ))));
            }
        }

        let device = opts.device.clone();
        let batch_timeout_ms = opts.batch_timeout_ms;
        let per_chunk_timeout_ms = opts.per_chunk_timeout_ms;
        let max_new_tokens = opts.max_new_tokens;

        let load = tokio::task::spawn_blocking(move || -> Result<CandleInner, LunarisError> {
            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-270m-it tokenizer: {e}"
                )))
            })?;
            let cfg_bytes = std::fs::read(&config_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-270m-it config: read {} ({e})",
                    config_path.display()
                )))
            })?;
            let cfg: Gemma3Config = serde_json::from_slice(&cfg_bytes).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-270m-it config: parse {} ({e})",
                    config_path.display()
                )))
            })?;
            let bytes = std::fs::read(&safetensors_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-270m-it weights: read {} ({e})",
                    safetensors_path.display()
                )))
            })?;
            let vb =
                VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "gemma-3-270m-it weights: {e}"
                    )))
                })?;
            let model = Gemma3Model::new(false, &cfg, vb).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-270m-it model construct: {e}"
                )))
            })?;

            Ok(CandleInner {
                tokenizer,
                model: Mutex::new(model),
                device,
                batch_timeout_ms,
                per_chunk_timeout_ms,
                max_new_tokens,
            })
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-270m-it join: {e}")))
        })??;

        Ok(Self { inner: Arc::new(load) })
    }

    fn verify_one(
        inner: &CandleInner,
        item: &NeedsReviewItem,
    ) -> Result<VerifyDecision, LunarisError> {
        let prompt = crate::arbitration_prompt(item);

        let encoding = inner.tokenizer.encode(prompt.as_str(), true).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-270m-it tokenize: {e}")))
        })?;
        let input_ids = encoding.get_ids().to_vec();
        if input_ids.is_empty() {
            return Ok(VerifyDecision::deferred());
        }

        let mut model_guard = inner.model.lock();
        let mut all_ids: Vec<u32> = input_ids;
        let mut emitted: Vec<u32> = Vec::with_capacity(inner.max_new_tokens);

        for step in 0..inner.max_new_tokens {
            let context_len = if step == 0 { all_ids.len() } else { 1 };
            let start = all_ids.len() - context_len;
            let ctx = &all_ids[start..];
            let input = Tensor::new(ctx, &inner.device).map_err(forward_err)?;
            let input = input.unsqueeze(0).map_err(forward_err)?;
            let logits = model_guard.forward(&input, start).map_err(forward_err)?;
            let last = logits
                .squeeze(0)
                .map_err(forward_err)?
                .narrow(0, ctx.len() - 1, 1)
                .map_err(forward_err)?
                .squeeze(0)
                .map_err(forward_err)?;
            let vocab: Vec<f32> = last.to_vec1::<f32>().map_err(forward_err)?;
            let next = vocab
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i as u32)
                .unwrap_or(0);
            if next == 1 {
                break;
            }
            emitted.push(next);
            all_ids.push(next);
        }
        drop(model_guard);

        let decoded = inner.tokenizer.decode(&emitted, true).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-270m-it detokenize: {e}")))
        })?;

        Ok(crate::parse_decision_json(&decoded, VerifierBackend::Candle))
    }
}

#[async_trait]
impl Verifier for CandleGemma3_270M {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        let inner = self.inner.clone();

        let timeout_ms = inner.batch_timeout_ms.max(inner.per_chunk_timeout_ms);
        let timeout = Duration::from_millis(timeout_ms);

        let item_owned = item.clone();
        let inner_for_task = inner.clone();
        let res = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                CandleGemma3_270M::verify_one(&inner_for_task, &item_owned)
            }),
        )
        .await;

        match res {
            Ok(Ok(Ok(d))) => Ok(d),
            Ok(Ok(Err(e))) => {
                tracing::warn!(err = %e, "gemma-3-270m-it verify failed; emitting deferred");
                Ok(VerifyDecision::deferred())
            }
            Ok(Err(join_err)) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "gemma-3-270m-it join: {join_err}"
            )))),
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = timeout_ms,
                    "gemma-3-270m-it verify timeout; emitting deferred"
                );
                Ok(VerifyDecision::deferred())
            }
        }
    }
}

#[inline]
fn forward_err(e: candle_core::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("gemma-3-270m-it forward: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_resolves_to_cache_subdir() {
        let opts = CandleGemma3_270MOpts::default();
        let path = opts.model_path.expect("default sets a path");
        let s = path.to_string_lossy().to_string();
        assert!(
            s.contains("lunaris") && s.contains("models") && s.contains("gemma-3-270m-it"),
            "default model_path should include the 270M cache layout, got: {s}"
        );
        assert_eq!(opts.batch_timeout_ms, DEFAULT_BATCH_TIMEOUT_MS);
        assert_eq!(opts.per_chunk_timeout_ms, DEFAULT_PER_CHUNK_TIMEOUT_MS);
        assert_eq!(opts.max_new_tokens, DEFAULT_MAX_NEW_TOKENS);
    }

    #[test]
    fn opts_default_batch_timeout_is_tight_for_270m() {
        // 270M is ~50x faster than 27B on CPU; the default batch timeout
        // is correspondingly tight. The 27B default is 1500 ms; 270M is
        // 200 ms (~7x tighter, conservative for the 2 GB laptop floor).
        assert_eq!(DEFAULT_BATCH_TIMEOUT_MS, 200);
        assert_eq!(DEFAULT_PER_CHUNK_TIMEOUT_MS, 100);
        assert_eq!(DEFAULT_MAX_NEW_TOKENS, 512);
    }

    #[tokio::test]
    async fn missing_weights_returns_actionable_error() {
        let tmp = std::env::temp_dir().join("lunaris-verify-270m-missing-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let opts = CandleGemma3_270MOpts {
            model_path: Some(tmp.clone()),
            ..CandleGemma3_270MOpts::default()
        };
        let err = CandleGemma3_270M::new(opts).await.expect_err("must error on missing weights");
        let msg = err.to_string();
        assert!(msg.contains("gemma-3-270m-it weights missing"), "got: {msg}");
        assert!(msg.contains("huggingface-cli download google/gemma-3-270m-it"), "got: {msg}");
        assert!(msg.contains("600MB"), "actionable disk hint must be present, got: {msg}");
    }
}
