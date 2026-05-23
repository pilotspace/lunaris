//! [`CandleGemma3_27B`] — Gemma-3 27B IT (instruction-tuned) verifier backed
//! by candle 0.10's typed `candle_transformers::models::gemma3::Model`.
//!
//! ## Phase 12b — thin wrapper
//!
//! This file was migrated from a full candle load/forward/decode
//! implementation to a thin wrapper around `Arc<dyn lunaris_llm::LlmBackend>`
//! + `lunaris_verify::LlmVerifier`. The public API (struct names, `*Opts`
//! types, constructor signatures, `Verifier` impl) is byte-identical on the
//! public surface to preserve downstream callers.
//!
//! B-12 fix: constructor shape mirrors `lunaris-extract::CandleGemma3_4B::new`
//! verbatim so Plan 04-04 `default_verifier` can call
//! `CandleGemma3_27B::new(CandleGemma3_27BOpts::default()).await`.
//!
//! ## Cache-miss contract (preserved)
//!
//! The constructor still performs the pre-flight existence check for
//! `tokenizer.json`, `config.json`, and `model.safetensors` and emits the
//! exact actionable error string:
//! ```text
//! gemma-3-27b-it weights missing at PATH — run `huggingface-cli download google/gemma-3-27b-it --local-dir PATH` (note: 27B requires ~16GB disk + ~24GB RAM for inference)
//! ```
//! The Plan 04-04 `default_verifier` catches this and substitutes
//! [`crate::NoopVerifier`] with `tracing::warn!`.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` (lib.rs) — delegates to `CandleBackend` which
//!   uses the safe `VarBuilder::from_buffered_safetensors`.
//! - All blocking work wrapped in `tokio::task::spawn_blocking` inside
//!   `CandleBackend::new` — the async runtime never stalls.
//! - No lock held across `.await` — lock discipline is inside `CandleBackend`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::Device;
use lunaris_core::{LunarisError, StorageError};
use lunaris_llm::{CandleBackend, CandleBackendOpts, LlmBackend};

use crate::Verifier;
use crate::llm_verifier::{LlmVerifier, LlmVerifierOpts};
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

/// Default sub-directory under the user's cache root. NOTE: 27B not 4B.
const DEFAULT_CACHE_SUBDIR: &str = "lunaris/models/gemma-3-27b-it";

/// Default per-batch timeout — 27B is ~7-10x slower than 4B so bumped 10x
/// from lunaris-extract's 150 ms default.
pub const DEFAULT_BATCH_TIMEOUT_MS: u64 = 1500;

/// Default per-chunk timeout for the fallback path. Generous — 27B per-chunk
/// forward pass is the slow path.
pub const DEFAULT_PER_CHUNK_TIMEOUT_MS: u64 = 800;

/// Default max-new-tokens cap. Arbitration explanations are longer than
/// entity extractions so bumped to 1024 (from 512 in lunaris-extract).
pub const DEFAULT_MAX_NEW_TOKENS: usize = 1024;

/// Construction options for [`CandleGemma3_27B`].
///
/// `Default` resolves `model_path` to
/// `~/.cache/lunaris/models/gemma-3-27b-it/`, `device` to `Device::Cpu`,
/// `batch_timeout_ms` to [`DEFAULT_BATCH_TIMEOUT_MS`], `per_chunk_timeout_ms`
/// to [`DEFAULT_PER_CHUNK_TIMEOUT_MS`], and `max_new_tokens` to
/// [`DEFAULT_MAX_NEW_TOKENS`].
#[derive(Clone, Debug)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_27BOpts {
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

impl Default for CandleGemma3_27BOpts {
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

/// Real Gemma-3 27B verifier. See module-level rustdoc for the v0 forward
/// strategy and failure-mode table.
///
/// Construction is async because tokenizer + safetensors load synchronously
/// hit the filesystem; we wrap the load in `tokio::task::spawn_blocking` to
/// avoid stalling the runtime on a cold cache. The shape mirrors
/// `lunaris-extract::CandleGemma3_4B` for B-12 consistency — Plan 04-04
/// `default_verifier` uses the same `Self::new(opts).await` entry point.
///
/// ## Phase 12b implementation
///
/// Internally holds a `LlmVerifier` wrapping a `CandleBackend`
/// (`lunaris-llm`). The public surface is unchanged.
#[derive(Clone)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_27B {
    inner: LlmVerifier,
}

impl std::fmt::Debug for CandleGemma3_27B {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleGemma3_27B").field("inner", &self.inner).finish()
    }
}

impl CandleGemma3_27B {
    /// Construct from the default cache. Mirrors
    /// `lunaris-extract::CandleGemma3_4B::try_new_from_default_cache`.
    pub async fn try_new_from_default_cache() -> Result<Self, LunarisError> {
        Self::new(CandleGemma3_27BOpts::default()).await
    }

    /// Construct from an explicit model path (escape hatch for tests + non-
    /// default cache layouts). Other options take their defaults.
    pub async fn try_new_from_path(model_path: PathBuf) -> Result<Self, LunarisError> {
        Self::new(CandleGemma3_27BOpts {
            model_path: Some(model_path),
            ..CandleGemma3_27BOpts::default()
        })
        .await
    }

    /// Construct with full options (B-12 — public `pub async fn new(opts)`
    /// mirroring `lunaris-extract::CandleGemma3_4B::new` verbatim so Plan
    /// 04-04 `default_verifier` can wire it the same way as the 4B extractor).
    ///
    /// Returns the actionable cache-miss error
    /// ```text
    /// gemma-3-27b-it weights missing at PATH — run `huggingface-cli download google/gemma-3-27b-it --local-dir PATH` (note: 27B requires ~16GB disk + ~24GB RAM for inference)
    /// ```
    /// when `tokenizer.json`, `config.json`, or `model.safetensors` are
    /// missing — the Plan 04-04 `default_verifier` catches this and
    /// substitutes [`crate::NoopVerifier`] with `tracing::warn!`.
    pub async fn new(opts: CandleGemma3_27BOpts) -> Result<Self, LunarisError> {
        let model_path = opts
            .model_path
            .clone()
            .unwrap_or_else(|| CandleGemma3_27BOpts::default().model_path.unwrap());

        // Pre-flight cache-miss check — must preserve exact error strings for
        // the actionable cache-miss message (Phase 12b contract: bit-for-bit
        // error strings preserved, including the "16GB" disk-hint).
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
                    "gemma-3-27b-it weights missing at {} (no {label}) — run `huggingface-cli download google/gemma-3-27b-it --local-dir {}` (note: 27B requires ~16GB disk + ~24GB RAM for inference)",
                    model_path.display(),
                    model_path.display()
                ))));
            }
        }

        // Build the unified CandleBackend (lunaris-llm).
        let backend_opts = CandleBackendOpts {
            model_name: "gemma-3-27b-it".to_string(),
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
impl Verifier for CandleGemma3_27B {
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
    use ulid::Ulid;

    #[test]
    fn opts_default_resolves_to_cache_subdir() {
        let opts = CandleGemma3_27BOpts::default();
        let path = opts.model_path.expect("default sets a path");
        let s = path.to_string_lossy().to_string();
        assert!(
            s.contains("lunaris") && s.contains("models") && s.contains("gemma-3-27b-it"),
            "default model_path should include the 27B cache layout, got: {s}"
        );
        assert_eq!(opts.batch_timeout_ms, DEFAULT_BATCH_TIMEOUT_MS);
        assert_eq!(opts.per_chunk_timeout_ms, DEFAULT_PER_CHUNK_TIMEOUT_MS);
        assert_eq!(opts.max_new_tokens, DEFAULT_MAX_NEW_TOKENS);
    }

    #[test]
    fn opts_default_batch_timeout_is_bumped_for_27b() {
        // 27B is 7-10x slower than 4B; the default batch timeout must be
        // bumped accordingly. lunaris-extract's 4B default is 150 ms; 27B is
        // 1500 ms (10x).
        assert_eq!(DEFAULT_BATCH_TIMEOUT_MS, 1500);
        assert_eq!(DEFAULT_PER_CHUNK_TIMEOUT_MS, 800);
    }

    #[tokio::test]
    async fn missing_weights_returns_actionable_error() {
        let tmp = std::env::temp_dir().join("lunaris-verify-27b-missing-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let opts = CandleGemma3_27BOpts {
            model_path: Some(tmp.clone()),
            ..CandleGemma3_27BOpts::default()
        };
        let err = CandleGemma3_27B::new(opts).await.expect_err("must error on missing weights");
        let msg = err.to_string();
        assert!(msg.contains("gemma-3-27b-it weights missing"), "got: {msg}");
        assert!(msg.contains("huggingface-cli download google/gemma-3-27b-it"), "got: {msg}");
        assert!(msg.contains("16GB"), "actionable disk hint must be present, got: {msg}");
    }

    #[test]
    fn parse_decision_json_extracts_arbitration() {
        let winner = Ulid::new();
        let loser = Ulid::new();
        let raw = format!(
            "Preamble {{\"winner_id\":\"{winner}\",\"loser_id\":\"{loser}\",\"reason\":\"conf higher\"}} trailing"
        );
        let d = crate::parse_decision_json(&raw, VerifierBackend::Candle);
        assert!(d.applies());
        assert_eq!(d.winner_id, Some(winner));
        assert_eq!(d.loser_id, Some(loser));
        assert_eq!(d.backend, VerifierBackend::Candle);
    }

    #[test]
    fn parse_decision_json_falls_back_to_deferred_on_bad_json() {
        let d = crate::parse_decision_json("{not json}", VerifierBackend::Candle);
        assert!(!d.applies());
    }

    #[test]
    fn parse_decision_json_handles_no_brace() {
        let d = crate::parse_decision_json("no json here", VerifierBackend::Candle);
        assert!(!d.applies());
    }
}
