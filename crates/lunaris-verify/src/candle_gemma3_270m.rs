//! [`CandleGemma3_270M`] — Gemma-3 270M IT verifier backed by candle 0.10's
//! typed `candle_transformers::models::gemma3::Model`.
//!
//! ## Phase 12b — thin wrapper
//!
//! This file was migrated from a full candle load/forward/decode
//! implementation to a thin wrapper around `Arc<dyn lunaris_llm::LlmBackend>`
//! + `lunaris_verify::LlmVerifier`. The public API (struct names, `*Opts`
//! types, constructor signatures, `Verifier` impl) is byte-identical on the
//! public surface to preserve downstream callers.
//!
//! Constructor shape (`new`, `try_new_from_default_cache`, `try_new_from_path`)
//! is preserved verbatim so Plan 04-04 `default_verifier` can call
//! `CandleGemma3_270M::new(CandleGemma3_270MOpts::default()).await` unchanged.
//!
//! ## Cache-miss contract (preserved)
//!
//! The constructor still performs the pre-flight existence check for
//! `tokenizer.json`, `config.json`, and `model.safetensors` and emits the
//! exact actionable error string:
//! ```text
//! gemma-3-270m-it weights missing at PATH — run `huggingface-cli download google/gemma-3-270m-it --local-dir PATH` (note: 270M requires ~600MB disk + ~1GB RAM for inference)
//! ```
//! `default_verifier` catches this and substitutes [`crate::NoopVerifier`]
//! with `tracing::warn!`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::Device;
use lunaris_core::LunarisError;
use lunaris_llm::{CandleBackend, CandleBackendOpts, LlmBackend};

use crate::Verifier;
use crate::llm_verifier::{LlmVerifier, LlmVerifierOpts};
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

// Re-export StorageError for the cache-miss error path.
use lunaris_core::StorageError;

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
///
/// ## Phase 12b implementation
///
/// Internally holds a `LlmVerifier` wrapping a `CandleBackend`
/// (`lunaris-llm`). The public surface is unchanged.
#[derive(Clone)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_270M {
    inner: LlmVerifier,
}

impl std::fmt::Debug for CandleGemma3_270M {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleGemma3_270M").field("inner", &self.inner).finish()
    }
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

        // Pre-flight cache-miss check — must preserve exact error strings for
        // the actionable cache-miss message (Phase 12b contract: bit-for-bit
        // error strings preserved, including the "600MB" disk-hint).
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

        // Build the unified CandleBackend (lunaris-llm).
        let backend_opts = CandleBackendOpts {
            model_name: "gemma-3-270m-it".to_string(),
            model_path,
            device: opts.device,
        };
        let backend: Arc<dyn LlmBackend> = Arc::new(CandleBackend::new(backend_opts).await?);

        // timeout = max(batch, per_chunk) — matches legacy verify path.
        let timeout_ms = opts.batch_timeout_ms.max(opts.per_chunk_timeout_ms);
        let verifier = LlmVerifier::with_opts(
            backend,
            LlmVerifierOpts {
                timeout_ms,
                max_tokens: opts.max_new_tokens as u32,
                temperature: 0.0,
                backend_tag: VerifierBackend::Candle,
                ..LlmVerifierOpts::default()
            },
        );

        Ok(Self { inner: verifier })
    }
}

#[async_trait]
impl Verifier for CandleGemma3_270M {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        self.inner.verify(item).await
    }

    fn applies(&self) -> bool {
        self.inner.applies()
    }
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
