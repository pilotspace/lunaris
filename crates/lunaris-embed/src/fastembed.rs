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
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
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
/// convention established in [`crate::ollama`].
pub const FASTEMBED_CACHE_DIR_ENV: &str = "LUNARIS_FASTEMBED_CACHE_DIR";

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
}

impl Default for FastembedEmbedderOpts {
    fn default() -> Self {
        Self {
            cache_dir: Some(resolve_default_cache_dir()),
            show_download_progress: false,
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
            .field("dim", &FASTEMBED_GEMMA_DIM)
            .field("cache_dir", &self.inner.cache_dir)
            .finish()
    }
}

struct Inner {
    /// The ONNX session. `embed` is `&mut self` so the lock IS the
    /// serialisation point — see module doc.
    model: Mutex<TextEmbedding>,
    /// Retained for `Debug` + future operator triage tracing. Not used in the
    /// hot path.
    cache_dir: PathBuf,
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
        let cache_dir = opts
            .cache_dir
            .unwrap_or_else(resolve_default_cache_dir);

        // T-19-01-03 mitigation: log model + cache_dir at INFO so operators can
        // diff env-to-env. Do NOT log inputs anywhere in this module
        // (T-19-01-04).
        tracing::info!(
            model = "EmbeddingGemma300M",
            cache_dir = %cache_dir.display(),
            "fastembed embedder constructing"
        );

        let init = InitOptions::new(EmbeddingModel::EmbeddingGemma300M)
            .with_cache_dir(cache_dir.clone())
            .with_show_download_progress(opts.show_download_progress);

        let model = TextEmbedding::try_new(init).map_err(anyhow_to_lunaris)?;

        Ok(Self {
            inner: Arc::new(Inner {
                model: Mutex::new(model),
                cache_dir,
            }),
        })
    }
}

#[async_trait]
impl Embedder for FastembedEmbedder {
    fn dim(&self) -> usize {
        FASTEMBED_GEMMA_DIM
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        // Move owned inputs across the spawn_blocking boundary — `&str`
        // borrows are not `'static` so we have to materialise `String`s.
        let owned: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let inner = self.inner.clone();

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
                if row.len() != FASTEMBED_GEMMA_DIM {
                    return Err(LunarisError::Storage(StorageError::Backend(format!(
                        "fastembed: dim mismatch — model returned {} dims, expected {FASTEMBED_GEMMA_DIM}",
                        row.len()
                    ))));
                }
                out.push(l2_normalize_row(row));
            }
            Ok(out)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("fastembed join: {e}")))
        })?
    }
}

/// L2-normalise a single row in place. If the row is degenerate
/// (`l2 < f64::EPSILON`) it is returned unchanged — matches
/// [`crate::candle_gemma`]'s behaviour and avoids dividing by zero.
#[inline]
fn l2_normalize_row(row: Vec<f32>) -> Vec<f32> {
    let l2 = row.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if l2 > f64::EPSILON {
        let mut out: Vec<f32> = row;
        for v in out.iter_mut() {
            *v = (*v as f64 / l2) as f32;
        }
        debug_assert_eq!(out.len(), FASTEMBED_GEMMA_DIM);
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
        let out = l2_normalize_row(row);
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
        let out = l2_normalize_row(row);
        assert_eq!(out.len(), FASTEMBED_GEMMA_DIM);
        assert!(out.iter().all(|&x| x == 0.0));
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
        let inputs = vec!["hello world", "lunaris memory engine"];
        let vecs = embedder
            .embed_batch(inputs.iter().copied().collect::<Vec<&str>>().as_slice())
            .await
            .expect("embed_batch");
        assert_eq!(vecs.len(), 2);
        for v in &vecs {
            assert_eq!(v.len(), FASTEMBED_GEMMA_DIM);
            let l2 = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            assert!((l2 - 1.0).abs() < 1e-3, "L2 norm = {l2}, expected ~ 1.0");
        }
    }
}
