//! `FastembedEmbedder` — ONNX-backed EmbeddingGemma 300M via fastembed-rs.
//!
//! ## v0 forward-pass strategy
//!
//! Where [`crate::candle_gemma::CandleEmbeddingGemma`] reads the
//! `embed_tokens.weight` matrix and mean-pools the first-layer token
//! embeddings (a pragmatic "lexical" shortcut), this backend runs the
//! **full ONNX forward pass** of `EmbeddingGemma300M` via `fastembed::TextEmbedding`
//! (which sits on top of `ort` 2.x — the ONNX Runtime Rust binding). The graph
//! emits `sentence_embedding` already mean-pooled inside the ONNX model; we
//! defensively L2-normalise on the way out because the graph is NOT guaranteed
//! to emit unit vectors across all model variants, and Moon `FT.SEARCH` cosine
//! distance requires unit-norm rows.
//!
//! Weights auto-download on first call to `FastembedEmbedder::new` via
//! `hf-hub` (TLS-enforced — `hf-hub-native-tls` feature). Cache directory
//! defaults to `~/.cache/lunaris/models/fastembed/` (shares parent with the
//! candle path's `~/.cache/lunaris/models/embedding-gemma-300m/`, so
//! `rm -rf ~/.cache/lunaris/models/` wipes both backends in one go).
//!
//! ## `&mut self` -> `&self` adapter
//!
//! `fastembed::TextEmbedding::embed` is `&mut self` and synchronous (CPU-bound
//! ORT call). The [`Embedder`] trait is `&self` and async. We bridge with
//! `Arc<Inner { Mutex<TextEmbedding> }>`:
//! - The Mutex is `parking_lot::Mutex` (CLAUDE.md lock discipline — never
//!   `std::sync::Mutex` for new code).
//! - The lock is acquired **inside** `tokio::task::spawn_blocking`, never held
//!   across `.await` (CLAUDE.md: "never hold a lock across `.await`").
//! - This serializes concurrent `embed_batch` calls per `FastembedEmbedder`
//!   instance. That's fine: fastembed batches internally at `batch_size = 256`
//!   and Lunaris ingest is single-writer-per-scope, so the Mutex never
//!   meaningfully contends. Concurrent readers wanting parallelism construct
//!   multiple `FastembedEmbedder` instances (one ORT session per instance).
//!
//! ## Defensive L2-normalize
//!
//! Each output row is L2-normalised on the host side, matching the candle
//! path's invariant (and the trait-level expectation). If `l2 < f64::EPSILON`
//! (a degenerate all-zeros graph output for an empty/pad-only input) we return
//! the row unchanged — same behaviour as `candle_gemma.rs`.
//!
//! ## Failure modes
//!
//! | Condition                                              | Returned error                                                                     |
//! |--------------------------------------------------------|-------------------------------------------------------------------------------------|
//! | HF Hub download failure (no network, 4xx, TLS)         | `LunarisError::Storage(StorageError::Backend("fastembed: ..."))` (anyhow rewrap)    |
//! | ORT session init failure (corrupt cache, bad ONNX)     | `LunarisError::Storage(StorageError::Backend("fastembed: ..."))`                    |
//! | `TextEmbedding::embed` call failure (tokenizer, ORT)   | `LunarisError::Storage(StorageError::Backend("fastembed: ..."))`                    |
//! | `tokio::task::spawn_blocking` join failure (panic)     | `LunarisError::Storage(StorageError::Backend("fastembed join: ..."))`               |
//! | First-call row width ≠ [`FASTEMBED_GEMMA_DIM`]         | `LunarisError::Storage(StorageError::Backend("fastembed: dim mismatch ..."))`       |
//! | Mutex poisoned                                         | Cannot occur — `parking_lot::Mutex` is poison-free by design.                       |

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fastembed::{
    EmbeddingModel, InitOptions, InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use lunaris_core::{Embedder, LunarisError, StorageError};
use parking_lot::Mutex;

/// Output dimensionality of `EmbeddingGemma300M`. Fixed at 768d — matches
/// [`crate::candle_gemma::EMBEDDING_GEMMA_DIM`] so the two backends are
/// drop-in replacements through the `Embedder` trait surface.
pub const FASTEMBED_GEMMA_DIM: usize = 768;

/// Maximum input tokens per request (EmbeddingGemma context window). Mirrors
/// [`crate::candle_gemma::EMBEDDING_GEMMA_MAX_TOKENS`] for parity; truncation
/// is handled inside fastembed's tokenizer wrapper (we don't need to truncate
/// on the host side as candle_gemma does).
pub const FASTEMBED_GEMMA_MAX_TOKENS: usize = 2048;

/// Environment variable that overrides the default fastembed cache directory.
/// Mirrors the `LUNARIS_OLLAMA_URL` / `LUNARIS_OLLAMA_MODEL` env-override
/// convention established in `crate::ollama` (feature-gated).
pub const FASTEMBED_CACHE_DIR_ENV: &str = "LUNARIS_FASTEMBED_CACHE_DIR";

// Phase 20 Plan 20-01 — execution-provider plumbing lives in a sibling module
// to keep this file under the project's split threshold. Re-exported so the
// public API surface (`lunaris_embed::fastembed::ExecutionPreference`) stays
// unchanged for downstream callers.
pub use crate::fastembed_exec::{
    ExecutionPreference, FASTEMBED_EXECUTION_ENV, execution_from_env, parse_execution,
};
use crate::fastembed_exec::{build_execution_providers, requests_accelerator};

/// Construction options for [`FastembedEmbedder`].
///
/// `Default` resolves `cache_dir` in priority order:
/// 1. `$LUNARIS_FASTEMBED_CACHE_DIR` if set (operator-controllable for CI / sandboxes);
/// 2. `~/.cache/lunaris/models/fastembed/` (shares parent with the candle cache);
/// 3. `./lunaris/models/fastembed/` as a last-ditch fallback when `dirs::cache_dir`
///    returns `None` (rare — only on platforms without a HOME concept).
///
/// `show_download_progress` defaults to `false` so server processes don't
/// spew progress bars into structured logs. Set `true` for local CLI use.
#[derive(Clone, Debug)]
pub struct FastembedEmbedderOpts {
    /// Filesystem path where fastembed stores auto-downloaded ONNX weights.
    /// `None` means "resolve via the env-override → `dirs::cache_dir()` chain
    /// at `Default` time"; once `Default` runs this is always `Some(...)`.
    pub cache_dir: Option<PathBuf>,
    /// Forwarded to `fastembed::InitOptions::with_show_download_progress`.
    /// Default `false` to keep server logs clean.
    pub show_download_progress: bool,
    /// ORT execution-provider preference (Phase 20 Plan 20-01). `Default`
    /// reads `$LUNARIS_FASTEMBED_EXECUTION`; unknown values resolve to `Cpu`
    /// with a `tracing::warn`. Set programmatically when callers want to
    /// override the environment.
    pub execution: ExecutionPreference,
}

impl Default for FastembedEmbedderOpts {
    fn default() -> Self {
        Self {
            cache_dir: Some(resolve_default_cache_dir()),
            show_download_progress: false,
            execution: execution_from_env(),
        }
    }
}

/// Resolve the default fastembed cache directory. See
/// [`FastembedEmbedderOpts`] doc for the precedence chain.
fn resolve_default_cache_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var(FASTEMBED_CACHE_DIR_ENV)
        && !env_dir.is_empty()
    {
        return PathBuf::from(env_dir);
    }
    let cache_root = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    cache_root.join("lunaris").join("models").join("fastembed")
}

/// ONNX-backed `EmbeddingGemma 300M` embedder. See module-level doc for the
/// adapter strategy and failure-mode table.
#[derive(Clone)]
pub struct FastembedEmbedder {
    /// Cheap-to-clone handle; the heavy ORT session lives inside the `Arc`.
    inner: Arc<Inner>,
}

impl std::fmt::Debug for FastembedEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastembedEmbedder")
            .field("dim", &self.inner.dim)
            .field("cache_dir", &self.inner.cache_dir)
            .finish()
    }
}

struct Inner {
    /// The ONNX session. `embed` is `&mut self` so the lock IS the
    /// serialisation point — see module doc.
    model: Mutex<TextEmbedding>,
    /// Retained for `Debug` + future operator triage tracing. Not used in the
    /// hot path. For the user-defined-model path this is `PathBuf::new()`
    /// (empty) since the operator hands us bytes directly — there is no
    /// on-disk cache by definition.
    cache_dir: PathBuf,
    /// Embedding dimensionality. For the default path this is
    /// [`FASTEMBED_GEMMA_DIM`]; for the user-defined path it is the
    /// operator-declared `dim` from [`FastembedUserDefinedOpts`].
    ///
    /// Made runtime (rather than the compile-time constant the Phase 19
    /// implementation read) by Plan 20-01 Task 3 so bring-your-own-model
    /// callers see their own dim through the [`Embedder`] trait surface.
    dim: usize,
}

impl FastembedEmbedder {
    /// Construct a real ONNX-backed embedder. On first call this triggers an
    /// HF Hub download of the EmbeddingGemma 300M weights (~600 MB) into
    /// `opts.cache_dir`; subsequent calls hit the cache.
    ///
    /// Construction is **synchronous** because fastembed's `try_new` is
    /// itself synchronous — the I/O happens inline. Callers that need to
    /// avoid stalling the runtime should wrap this in
    /// `tokio::task::spawn_blocking` at the call site; we deliberately do
    /// NOT wrap inside `new` so the error mapping stays straightforward and
    /// the caller controls the spawn context.
    pub fn new(opts: FastembedEmbedderOpts) -> Result<Self, LunarisError> {
        let cache_dir = opts.cache_dir.unwrap_or_else(resolve_default_cache_dir);
        let execution = opts.execution.clone();

        // T-19-01-03 mitigation: log model + cache_dir at INFO so operators can
        // diff env-to-env. Do NOT log inputs anywhere in this module
        // (T-19-01-04).
        tracing::info!(
            backend = "fastembed",
            model = "EmbeddingGemma300M",
            cache_dir = %cache_dir.display(),
            execution = ?execution,
            "fastembed embedder constructing"
        );

        let build = |providers_enabled: bool| -> Result<TextEmbedding, anyhow::Error> {
            let mut init = InitOptions::new(EmbeddingModel::EmbeddingGemma300M)
                .with_cache_dir(cache_dir.clone())
                .with_show_download_progress(opts.show_download_progress);
            if providers_enabled {
                init = init.with_execution_providers(build_execution_providers(&execution));
            }
            TextEmbedding::try_new(init)
        };

        let model = try_with_fallback(&execution, build)?;

        // Best-effort label: fastembed's `Session` doesn't expose the active
        // EP, so we report the requested preference here. The fallback path
        // emits its own `warn` if it kicked in, which is the durable signal
        // for "you asked for accelerator but got CPU".
        let resolved = execution.clone();
        tracing::info!(
            backend = "fastembed",
            model = "EmbeddingGemma300M",
            execution = ?resolved,
            "fastembed embedder initialized"
        );

        Ok(Self {
            inner: Arc::new(Inner {
                model: Mutex::new(model),
                cache_dir,
                dim: FASTEMBED_GEMMA_DIM,
            }),
        })
    }

    /// Bring-your-own ONNX model (Phase 20 Plan 20-01). The operator supplies
    /// the model bytes + tokenizer bytes in [`FastembedUserDefinedOpts`] and
    /// declares the output dimensionality (`dim`); the constructor wires
    /// fastembed's [`UserDefinedEmbeddingModel`] / `InitOptionsUserDefined`
    /// and returns a ready embedder.
    ///
    /// # Trust requirement
    ///
    /// The ONNX bytes execute in-process through ONNX Runtime. They MUST come
    /// from a trusted source (operator-controlled model registry, not
    /// user-uploaded content) — Lunaris performs no graph validation. See
    /// `.planning/phases/20-fastembed-adoption/20-01-PLAN.md` threat
    /// `T-20-01-01`.
    ///
    /// # Storage-dim constraint
    ///
    /// Lunaris's default storage schema is **768-d** (Moon FT index + Postgres
    /// `vector(768)` column). Operators bringing a model whose `dim != 768`
    /// MUST also reindex storage — this is the storage-side migration covered
    /// by Plan 20-03. Lunaris does NOT enforce dim parity between embedder
    /// and storage on the hot path; a mismatch surfaces as a backend insert
    /// error at first ingest.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use lunaris_embed::fastembed::{
    ///     FastembedEmbedder, FastembedUserDefinedOpts, PoolingMode, ExecutionPreference,
    /// };
    ///
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let onnx = std::fs::read("models/helios-finetuned.onnx")?;
    /// let tok = std::fs::read("models/helios-finetuned/tokenizer.json")?;
    /// let embedder = FastembedEmbedder::from_user_defined(FastembedUserDefinedOpts {
    ///     onnx_file: onnx,
    ///     tokenizer_file: tok,
    ///     tokenizer_config_file: None,
    ///     special_tokens_map_file: None,
    ///     config_file: None,
    ///     dim: 1024, // MUST match the ONNX model's output dim
    ///     pooling: PoolingMode::Mean,
    ///     execution: ExecutionPreference::Cpu,
    ///     max_length: 2048,
    /// })?;
    /// // let lunaris = Lunaris::open(url).await?.with_embedder(Arc::new(embedder));
    /// let _ = Arc::new(embedder);
    /// # Ok(()) }
    /// ```
    pub fn from_user_defined(opts: FastembedUserDefinedOpts) -> Result<Self, LunarisError> {
        if opts.onnx_file.is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(
                "fastembed: from_user_defined called with empty onnx_file bytes".to_string(),
            )));
        }
        if opts.tokenizer_file.is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(
                "fastembed: from_user_defined called with empty tokenizer_file bytes".to_string(),
            )));
        }
        if opts.dim == 0 {
            return Err(LunarisError::Storage(StorageError::Backend(
                "fastembed: from_user_defined called with dim = 0".to_string(),
            )));
        }

        let execution = opts.execution.clone();
        let dim = opts.dim;
        let max_length = opts.max_length;

        tracing::info!(
            backend = "fastembed",
            model = "user-defined",
            dim,
            execution = ?execution,
            "fastembed user-defined embedder constructing"
        );

        // The struct is non-`Clone` once we move bytes in. Construct once;
        // fallback retry below requires a second model — for the user-defined
        // path we keep buffers around in `Option<...>` so the fallback path
        // can reuse them without double-copying multi-MB onnx blobs.
        let user_model = UserDefinedEmbeddingModel {
            onnx_file: opts.onnx_file,
            external_initializers: Vec::new(),
            tokenizer_files: TokenizerFiles {
                tokenizer_file: opts.tokenizer_file,
                config_file: opts.config_file.unwrap_or_default(),
                special_tokens_map_file: opts.special_tokens_map_file.unwrap_or_default(),
                tokenizer_config_file: opts.tokenizer_config_file.unwrap_or_default(),
            },
            pooling: Some(opts.pooling.into()),
            quantization: QuantizationMode::None,
            output_key: None,
        };

        let model = try_user_defined_with_fallback(&execution, user_model, max_length)?;

        // Best-effort label: fastembed's `Session` doesn't expose the active
        // EP, so we report the requested preference here. The fallback path
        // emits its own `warn` if it kicked in, which is the durable signal
        // for "you asked for accelerator but got CPU".
        let resolved = execution.clone();
        tracing::info!(
            backend = "fastembed",
            model = "user-defined",
            dim,
            execution = ?resolved,
            "fastembed user-defined embedder initialized"
        );

        Ok(Self {
            inner: Arc::new(Inner { model: Mutex::new(model), cache_dir: PathBuf::new(), dim }),
        })
    }
}

/// Options for [`FastembedEmbedder::from_user_defined`]. All byte buffers are
/// moved into the constructor — they aren't retained inside the embedder once
/// the ONNX session has been built (the session owns its parsed graph).
///
/// **Storage-side dim invariant:** see the constructor's rustdoc — `dim` must
/// match the ONNX model's output AND should match Lunaris's storage schema
/// (default 768) unless storage is reindexed.
#[derive(Clone, Debug)]
pub struct FastembedUserDefinedOpts {
    /// Raw bytes of the ONNX graph (e.g., `model.onnx`).
    pub onnx_file: Vec<u8>,
    /// Raw bytes of the HF-format `tokenizer.json`.
    pub tokenizer_file: Vec<u8>,
    /// Optional `tokenizer_config.json` bytes. Empty if `None`.
    pub tokenizer_config_file: Option<Vec<u8>>,
    /// Optional `special_tokens_map.json` bytes.
    pub special_tokens_map_file: Option<Vec<u8>>,
    /// Optional model `config.json` bytes (architecture metadata).
    pub config_file: Option<Vec<u8>>,
    /// Output dimensionality declared by the operator. MUST match what the
    /// ONNX graph actually emits; a mismatch surfaces as a vector-index
    /// rejection at the first ingest call.
    pub dim: usize,
    /// Pooling strategy applied to token-level embeddings to produce the
    /// sentence vector. Mirrors fastembed's [`Pooling`] enum.
    pub pooling: PoolingMode,
    /// ORT execution provider preference (same enum as the default path).
    pub execution: ExecutionPreference,
    /// Token context window. Defaults to 2048 to match `EmbeddingGemma300M`.
    pub max_length: usize,
}

impl Default for FastembedUserDefinedOpts {
    fn default() -> Self {
        Self {
            onnx_file: Vec::new(),
            tokenizer_file: Vec::new(),
            tokenizer_config_file: None,
            special_tokens_map_file: None,
            config_file: None,
            dim: 0,
            pooling: PoolingMode::Mean,
            execution: execution_from_env(),
            max_length: FASTEMBED_GEMMA_MAX_TOKENS,
        }
    }
}

/// Lunaris-facing pooling enum — decouples callers from a direct
/// [`fastembed::Pooling`] type dependency.
///
/// `Cls` mirrors fastembed's BERT-style first-token pooling; `Mean` is the
/// recommended setting for sentence-similarity models (EmbeddingGemma + most
/// BGE variants).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PoolingMode {
    /// CLS-token pooling (BERT-style). Maps to [`fastembed::Pooling::Cls`].
    Cls,
    /// Mean pooling with attention-mask weighting. Maps to
    /// [`fastembed::Pooling::Mean`].
    #[default]
    Mean,
}

impl From<PoolingMode> for Pooling {
    fn from(m: PoolingMode) -> Self {
        match m {
            PoolingMode::Cls => Pooling::Cls,
            PoolingMode::Mean => Pooling::Mean,
        }
    }
}

/// Try the construction closure with execution providers; on failure when an
/// accelerator was requested, retry once with CPU only and a `tracing::warn`.
fn try_with_fallback<F>(
    pref: &ExecutionPreference,
    mut build: F,
) -> Result<TextEmbedding, LunarisError>
where
    F: FnMut(bool) -> Result<TextEmbedding, anyhow::Error>,
{
    let want_accelerator = requests_accelerator(pref);
    match build(want_accelerator) {
        Ok(m) => Ok(m),
        Err(e) if want_accelerator => {
            // T-20-01-03 mitigation: %e (Display) — don't dump full provider
            // debug context (which may include driver paths) into logs.
            tracing::warn!(
                error = %e,
                requested = ?pref,
                "fastembed execution provider init failed, falling back to CPU"
            );
            build(false).map_err(anyhow_to_lunaris)
        }
        Err(e) => Err(anyhow_to_lunaris(e)),
    }
}

/// User-defined variant of [`try_with_fallback`]. Owns the
/// `UserDefinedEmbeddingModel` so the fallback retry doesn't have to clone
/// multi-MB byte buffers — fastembed's struct is `Clone`, so we keep the
/// owned copy in scope and pass clones in.
fn try_user_defined_with_fallback(
    pref: &ExecutionPreference,
    user_model: UserDefinedEmbeddingModel,
    max_length: usize,
) -> Result<TextEmbedding, LunarisError> {
    let want_accelerator = requests_accelerator(pref);
    let build = |providers_enabled: bool, m: UserDefinedEmbeddingModel| {
        let mut init = InitOptionsUserDefined::new().with_max_length(max_length);
        if providers_enabled {
            init = init.with_execution_providers(build_execution_providers(pref));
        }
        TextEmbedding::try_new_from_user_defined(m, init)
    };

    if want_accelerator {
        // Keep an unconsumed clone to retry on the CPU path if the accelerator
        // session-build fails.
        let retry_model = user_model.clone();
        match build(true, user_model) {
            Ok(m) => Ok(m),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    requested = ?pref,
                    "fastembed (user-defined) execution provider init failed, falling back to CPU"
                );
                build(false, retry_model).map_err(anyhow_to_lunaris)
            }
        }
    } else {
        build(false, user_model).map_err(anyhow_to_lunaris)
    }
}

#[async_trait]
impl Embedder for FastembedEmbedder {
    fn dim(&self) -> usize {
        // Phase 20 Plan 20-01 Task 3 — read runtime dim from Inner. For the
        // default `new()` path this is `FASTEMBED_GEMMA_DIM` (768); for the
        // `from_user_defined` path it is operator-declared.
        self.inner.dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        // Move owned inputs across the spawn_blocking boundary — `&str`
        // borrows are not `'static` so we have to materialise `String`s.
        let owned: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let inner = self.inner.clone();
        let expected_dim = inner.dim;

        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, LunarisError> {
            // Acquire the Mutex INSIDE the blocking closure. CLAUDE.md lock
            // discipline: never across `.await`. `parking_lot::Mutex` is
            // poison-free so the unwrap-like `lock()` cannot fail.
            let raw: Vec<Vec<f32>> = {
                let mut guard = inner.model.lock();
                // `None` -> use fastembed's default batch size (256).
                guard.embed(owned, None).map_err(anyhow_to_lunaris)?
            }; // guard drops here; subsequent normalisation is lock-free.

            let mut out: Vec<Vec<f32>> = Vec::with_capacity(raw.len());
            for row in raw.into_iter() {
                if row.len() != expected_dim {
                    return Err(LunarisError::Storage(StorageError::Backend(format!(
                        "fastembed: dim mismatch — model returned {} dims, expected {expected_dim}",
                        row.len()
                    ))));
                }
                out.push(l2_normalize_row(row, expected_dim));
            }
            Ok(out)
        })
        .await
        .map_err(|e| LunarisError::Storage(StorageError::Backend(format!("fastembed join: {e}"))))?
    }
}

/// L2-normalise a single row in place. If the row is degenerate
/// (`l2 < f64::EPSILON`) it is returned unchanged — matches
/// [`crate::candle_gemma`]'s behaviour and avoids dividing by zero.
///
/// `expected_dim` is passed for the debug-assert only; the function is
/// dim-agnostic post Phase 20 Plan 20-01 (the user-defined model path may
/// have `dim != 768`).
#[inline]
fn l2_normalize_row(row: Vec<f32>, expected_dim: usize) -> Vec<f32> {
    let l2 = row.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if l2 > f64::EPSILON {
        let mut out: Vec<f32> = row;
        for v in out.iter_mut() {
            *v = (*v as f64 / l2) as f32;
        }
        debug_assert_eq!(out.len(), expected_dim);
        out
    } else {
        row
    }
}

/// Bridge `anyhow::Error` (fastembed's error surface) to `LunarisError`.
/// Mirrors the candle path's `candle_err` helper.
#[inline]
fn anyhow_to_lunaris(e: anyhow::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("fastembed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_resolves_to_cache_subdir() {
        // Guard against env pollution from sibling tests (or the shell).
        // Use `unsafe`? No — std::env::remove_var is unsafe in edition 2024;
        // we work around by snapshotting and restoring around the assertion.
        // Easier: assert the *suffix* path components are right and just
        // skip the assertion if the env var is set externally (operator-set
        // overrides are explicitly allowed by the API contract).
        let env_override = std::env::var(FASTEMBED_CACHE_DIR_ENV).ok();
        if env_override.is_some() {
            // Operator override active — Default returns that, by contract.
            return;
        }
        let opts = FastembedEmbedderOpts::default();
        let path = opts.cache_dir.expect("default sets a cache_dir");
        let s = path.to_string_lossy().to_string();
        assert!(
            s.contains("lunaris") && s.contains("models") && s.contains("fastembed"),
            "default cache_dir should include the v0 cache layout, got: {s}"
        );
    }

    #[test]
    fn dim_constant_is_768() {
        assert_eq!(FASTEMBED_GEMMA_DIM, 768);
    }

    #[test]
    fn l2_normalize_unit_vector() {
        // Construct a non-unit vector; expect ‖result‖₂ ≈ 1.
        let mut row = vec![0.0_f32; FASTEMBED_GEMMA_DIM];
        row[0] = 3.0;
        row[1] = 4.0; // ‖row‖₂ = 5
        let out = l2_normalize_row(row, FASTEMBED_GEMMA_DIM);
        let l2 = out.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        assert!((l2 - 1.0).abs() < 1e-6, "expected unit norm, got {l2}");
        // 3/5 = 0.6, 4/5 = 0.8 — exact in f32.
        assert!((out[0] - 0.6).abs() < 1e-6);
        assert!((out[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_degenerate_row_returned_as_is() {
        // All-zero row: norm < EPSILON → returned unchanged (matches
        // candle_gemma).
        let row = vec![0.0_f32; FASTEMBED_GEMMA_DIM];
        let out = l2_normalize_row(row, FASTEMBED_GEMMA_DIM);
        assert_eq!(out.len(), FASTEMBED_GEMMA_DIM);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    // ---- Phase 20 Plan 20-01 ------------------------------------------------
    // ExecutionPreference + parse_execution tests live alongside their
    // implementation in `crate::fastembed_exec`. Tests below cover the parts
    // of Plan 20-01 that touch the embedder construction surface specifically:
    // from_user_defined error paths + PoolingMode mapping.

    #[test]
    fn from_user_defined_empty_onnx_returns_actionable_error() {
        // Empty bytes path — the constructor short-circuits BEFORE calling
        // into fastembed/ORT (so this test is offline-runnable). The error
        // string MUST contain `"fastembed"` so operators can grep for it.
        let opts = FastembedUserDefinedOpts {
            onnx_file: Vec::new(),
            tokenizer_file: vec![0u8; 4],
            dim: 768,
            ..Default::default()
        };
        let err = FastembedEmbedder::from_user_defined(opts).expect_err("empty onnx");
        let msg = format!("{err}");
        assert!(
            msg.contains("fastembed") && msg.contains("onnx_file"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn from_user_defined_empty_tokenizer_returns_actionable_error() {
        let opts = FastembedUserDefinedOpts {
            onnx_file: vec![0u8; 4],
            tokenizer_file: Vec::new(),
            dim: 768,
            ..Default::default()
        };
        let err = FastembedEmbedder::from_user_defined(opts).expect_err("empty tokenizer");
        let msg = format!("{err}");
        assert!(
            msg.contains("fastembed") && msg.contains("tokenizer_file"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn from_user_defined_zero_dim_returns_actionable_error() {
        let opts = FastembedUserDefinedOpts {
            onnx_file: vec![0u8; 4],
            tokenizer_file: vec![0u8; 4],
            dim: 0,
            ..Default::default()
        };
        let err = FastembedEmbedder::from_user_defined(opts).expect_err("zero dim");
        let msg = format!("{err}");
        assert!(msg.contains("fastembed") && msg.contains("dim"), "unexpected: {msg}");
    }

    #[test]
    fn from_user_defined_bad_onnx_bytes_surfaces_fastembed_error() {
        // Non-empty but invalid ONNX bytes — passes our front-door validation
        // and hits fastembed/ORT proper, which rejects them. The error MUST
        // be a `LunarisError::Storage(StorageError::Backend(..))` containing
        // the `"fastembed"` substring.
        let opts = FastembedUserDefinedOpts {
            onnx_file: b"not-a-real-onnx-graph".to_vec(),
            tokenizer_file: b"not-a-real-tokenizer".to_vec(),
            dim: 768,
            ..Default::default()
        };
        let err = FastembedEmbedder::from_user_defined(opts).expect_err("bad bytes");
        let msg = format!("{err}");
        assert!(msg.contains("fastembed"), "expected fastembed-prefixed error, got: {msg}");
    }

    #[test]
    fn pooling_mode_maps_to_fastembed_pooling() {
        let cls: Pooling = PoolingMode::Cls.into();
        assert!(matches!(cls, Pooling::Cls));
        let mean: Pooling = PoolingMode::Mean.into();
        assert!(matches!(mean, Pooling::Mean));
    }
}

// -----------------------------------------------------------------------------
// `embedder-it`-gated real-model smoke. Auto-downloads ~600 MB of ONNX weights
// on first run (30-90s cold; subsequent runs hit the cache in `~/.cache/
// lunaris/models/fastembed/embeddinggemma-300m-onnx/`). Verify by deleting
// that subdir and re-running — fastembed re-downloads transparently. Not
// included in the default test run; CI's existing `embedder-it` job picks
// this up automatically and Plan 19-02 expands the matrix.
// -----------------------------------------------------------------------------
#[cfg(all(test, feature = "embedder-it"))]
mod live_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fastembed_loads_real_model_and_embeds_one_batch() {
        let embedder = FastembedEmbedder::new(FastembedEmbedderOpts::default())
            .expect("real model load — auto-download to ~/.cache/lunaris/models/fastembed/");
        assert_eq!(embedder.dim(), FASTEMBED_GEMMA_DIM);
        let inputs: [&str; 2] = ["hello world", "lunaris memory engine"];
        let vecs = embedder.embed_batch(&inputs).await.expect("embed_batch");
        assert_eq!(vecs.len(), 2);
        for v in &vecs {
            assert_eq!(v.len(), FASTEMBED_GEMMA_DIM);
            let l2 = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            assert!((l2 - 1.0).abs() < 1e-3, "L2 norm = {l2}, expected ~ 1.0");
        }
    }
}
