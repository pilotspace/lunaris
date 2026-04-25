//! [`CandleGemma3_27B`] — Gemma-3 27B IT (instruction-tuned) verifier backed
//! by candle 0.10's typed `candle_transformers::models::gemma3::Model`.
//!
//! ## Wiring strategy
//!
//! This file is the Phase 4 slow-path verifier backend (D-01 + blueprint
//! §5.1). The backend mirrors `lunaris-extract::CandleGemma3_4B` with these
//! divergences:
//!
//! - Model weights path: `~/.cache/lunaris/models/gemma-3-27b-it/` (NOT the
//!   `gemma-3-4b-it` subdir). The cache-miss error message carries the
//!   actionable `huggingface-cli download google/gemma-3-27b-it` hint plus
//!   the disk + RAM headroom warning (27B takes ~16GB disk / ~24GB RAM).
//! - Per-batch timeout bumped 10x to 1500 ms (D-02 equivalent); per-chunk
//!   fallback bumped to 800 ms (27B is 7-10x slower than 4B on CPU).
//! - `DEFAULT_MAX_NEW_TOKENS = 1024` — arbitration explanations are longer
//!   than entity extractions.
//!
//! B-12 fix: constructor shape mirrors `lunaris-extract::CandleGemma3_4B::new`
//! verbatim so Plan 04-04 `default_verifier` can call
//! `CandleGemma3_27B::new(CandleGemma3_27BOpts::default()).await`.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` (lib.rs) — uses the safe
//!   `VarBuilder::from_buffered_safetensors` instead of the unsafe
//!   `from_mmaped_safetensors`.
//! - All blocking work wrapped in `tokio::task::spawn_blocking` so the async
//!   runtime never stalls on cold model load OR per-call forward pass.
//! - `parking_lot::Mutex<Gemma3Model>` for the model handle (because
//!   `forward(&mut self)` needs `&mut`); the lock is taken INSIDE
//!   `spawn_blocking` so the "never hold a lock across `.await`" rule is
//!   upheld trivially.
//!
//! ## Failure modes
//!
//! | Condition                                          | Returned error                                                                                                                  |
//! |----------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------|
//! | `model_path/tokenizer.json` missing                | `LunarisError::Storage(StorageError::Backend("gemma-3-27b-it weights missing at <path> — run `huggingface-cli download ...`"))` |
//! | `model_path/config.json` missing                   | same shape                                                                                                                       |
//! | `model_path/model.safetensors` missing             | same shape                                                                                                                       |
//! | tokenizer load failure                             | `LunarisError::Storage(StorageError::Backend("gemma-3-27b-it tokenizer: ..."))`                                                  |
//! | safetensors load failure                           | `LunarisError::Storage(StorageError::Backend("gemma-3-27b-it weights: ..."))`                                                    |
//! | candle tensor op failure during forward            | `LunarisError::Storage(StorageError::Backend("gemma-3-27b-it forward: ..."))`                                                    |
//! | tokio `spawn_blocking` join failure                | `LunarisError::Storage(StorageError::Backend("gemma-3-27b-it join: ..."))`                                                       |
//! | per-chunk timeout (D-02 equivalent)                | returns `VerifyDecision::deferred()` with a `tracing::warn!`                                                                     |

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
#[derive(Clone)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_27B {
    inner: Arc<CandleInner>,
}

impl std::fmt::Debug for CandleGemma3_27B {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleGemma3_27B")
            .field("device", &format_args!("{:?}", self.inner.device))
            .field("max_new_tokens", &self.inner.max_new_tokens)
            .field("batch_timeout_ms", &self.inner.batch_timeout_ms)
            .field("per_chunk_timeout_ms", &self.inner.per_chunk_timeout_ms)
            .finish()
    }
}

struct CandleInner {
    tokenizer: Tokenizer,
    /// Gemma-3 model. Held under a parking_lot Mutex because
    /// `Gemma3Model::forward(&mut self, ...)` requires `&mut`. The lock is
    /// taken inside `spawn_blocking` so the CLAUDE.md "never hold lock across
    /// await" rule is trivially upheld (the .await happens at the
    /// spawn_blocking boundary, not inside the closure).
    model: Mutex<Gemma3Model>,
    device: Device,
    batch_timeout_ms: u64,
    per_chunk_timeout_ms: u64,
    max_new_tokens: usize,
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

        let device = opts.device.clone();
        let batch_timeout_ms = opts.batch_timeout_ms;
        let per_chunk_timeout_ms = opts.per_chunk_timeout_ms;
        let max_new_tokens = opts.max_new_tokens;

        let load = tokio::task::spawn_blocking(move || -> Result<CandleInner, LunarisError> {
            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-27b-it tokenizer: {e}"
                )))
            })?;
            let cfg_bytes = std::fs::read(&config_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-27b-it config: read {} ({e})",
                    config_path.display()
                )))
            })?;
            let cfg: Gemma3Config = serde_json::from_slice(&cfg_bytes).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-27b-it config: parse {} ({e})",
                    config_path.display()
                )))
            })?;
            let bytes = std::fs::read(&safetensors_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-27b-it weights: read {} ({e})",
                    safetensors_path.display()
                )))
            })?;
            let vb =
                VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "gemma-3-27b-it weights: {e}"
                    )))
                })?;
            let model = Gemma3Model::new(false, &cfg, vb).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-27b-it model construct: {e}"
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
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-27b-it join: {e}")))
        })??;

        Ok(Self { inner: Arc::new(load) })
    }

    /// Internal — verify one item synchronously inside `spawn_blocking`.
    ///
    /// v0 strategy: prompt the model with the item JSON + arbitration
    /// instruction, greedy-sample up to `max_new_tokens` tokens, decode,
    /// post-hoc parse `{"winner_id": ..., "reason": ...}`. On any parse
    /// failure or missing ulid we return `VerifyDecision::deferred()` with
    /// a tracing::warn — the worker treats this as abstain.
    fn verify_one(
        inner: &CandleInner,
        item: &NeedsReviewItem,
    ) -> Result<VerifyDecision, LunarisError> {
        let prompt = crate::arbitration_prompt(item);

        let encoding = inner.tokenizer.encode(prompt.as_str(), true).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-27b-it tokenize: {e}")))
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
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-27b-it detokenize: {e}")))
        })?;

        Ok(crate::parse_decision_json(&decoded, VerifierBackend::Candle))
    }
}

#[async_trait]
impl Verifier for CandleGemma3_27B {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        let inner = self.inner.clone();

        // Per-chunk (single-item) timeout. The 27B path is always one item
        // per call so we don't mirror extract's per-batch vs per-chunk
        // split — instead we apply the max(batch, per_chunk) budget.
        let timeout_ms = inner.batch_timeout_ms.max(inner.per_chunk_timeout_ms);
        let timeout = Duration::from_millis(timeout_ms);

        let item_owned = item.clone();
        let inner_for_task = inner.clone();
        let res = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                CandleGemma3_27B::verify_one(&inner_for_task, &item_owned)
            }),
        )
        .await;

        match res {
            Ok(Ok(Ok(d))) => Ok(d),
            Ok(Ok(Err(e))) => {
                tracing::warn!(err = %e, "gemma-3-27b-it verify failed; emitting deferred");
                Ok(VerifyDecision::deferred())
            }
            Ok(Err(join_err)) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "gemma-3-27b-it join: {join_err}"
            )))),
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = timeout_ms,
                    "gemma-3-27b-it verify timeout; emitting deferred"
                );
                Ok(VerifyDecision::deferred())
            }
        }
    }
}

#[inline]
fn forward_err(e: candle_core::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("gemma-3-27b-it forward: {e}")))
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
