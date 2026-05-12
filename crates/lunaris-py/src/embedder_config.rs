//! Phase 21 Plan 21-01 — handwritten `EmbedderConfig` for the Python SDK.
//!
//! This module is **NOT codegen-managed** (mirrors the `scope.rs` /
//! `toggles.rs` precedent). The codegen IR models the 15-item method surface,
//! not builder-style construction. `EmbedderConfig` is an opaque holder around
//! a resolved `Arc<dyn Embedder>` that the Python SDK passes back into
//! `Lunaris.open(url, embedder=cfg)`.
//!
//! ## FFI cliff
//!
//! Python callers CANNOT implement the [`Embedder`] trait directly — that
//! would require per-call Python→Rust callbacks, which are slow and brittle.
//! The factory methods below cover every customization the Rust crate
//! supports MINUS the "roll your own trait impl" escape hatch (Rust-only).
//! See `.planning/phases/21-sdk-embedder-config/21-CONTEXT.md` for the
//! decision record.
//!
//! ## Factory methods
//!
//! - [`EmbedderConfig::fastembed`] — preset ONNX-backed EmbeddingGemma 300M
//!   via fastembed-rs (Phase 19-01).
//! - [`EmbedderConfig::from_onnx_bytes`] — bring-your-own ONNX model from
//!   in-memory bytes (Phase 20-01).
//! - [`EmbedderConfig::from_onnx_path`] — same as above but reads the bytes
//!   from disk (the SDK does the `std::fs::read`).
//! - [`EmbedderConfig::ollama`] — HTTP-backed Ollama embedder.
//!
//! ## Dim validation (T-21-01-04 mitigation)
//!
//! BYO-ONNX factories ([`from_onnx_bytes`] / [`from_onnx_path`]) wrap the
//! result in a [`DimValidatingEmbedder`] that asserts the observed dim
//! matches the operator-declared `dim` on the first `embed_batch` call. A
//! mismatch surfaces as a clear `LunarisError` instead of silent vector-index
//! corruption. Preset paths (`fastembed`, `ollama`) skip this wrapper —
//! their dim is known from the backend constants.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use lunaris_core::{Embedder, LunarisError, StorageError};
use lunaris_embed::fastembed::{
    ExecutionPreference, FASTEMBED_GEMMA_MAX_TOKENS, FastembedEmbedder, FastembedEmbedderOpts,
    FastembedUserDefinedOpts, PoolingMode,
};
use lunaris_embed::ollama::{OllamaEmbedder, OllamaEmbedderOpts};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::errors::py_err;

// ---------------------------------------------------------------------------
// EmbedderConfig
// ---------------------------------------------------------------------------

/// Opaque holder for a resolved [`Embedder`] backend.
///
/// Construct via the [`EmbedderConfig::fastembed`] /
/// [`EmbedderConfig::from_onnx_bytes`] / [`EmbedderConfig::from_onnx_path`] /
/// [`EmbedderConfig::ollama`] static methods, then pass to
/// `Lunaris.open(url, embedder=cfg)`.
///
/// ```python
/// from lunaris import EmbedderConfig, open
///
/// cfg = EmbedderConfig.fastembed(execution="coreml")
/// handle = await open(url, embedder=cfg)
/// ```
///
/// The inner `Arc<dyn Embedder>` is `pub(crate)` so the
/// `lunaris_with_embedder` free function in `lib.rs` can hand it to
/// `Lunaris::with_embedder`. Python callers cannot reach the inner Arc.
#[pyclass(frozen, name = "EmbedderConfig", module = "lunaris")]
#[derive(Clone)]
pub struct EmbedderConfig {
    pub(crate) inner: Arc<dyn Embedder>,
    /// Human-readable backend tag for `__repr__`.
    pub(crate) backend: &'static str,
    /// Dimensionality the backend reports. Used by `__repr__`.
    pub(crate) dim: usize,
}

#[pymethods]
impl EmbedderConfig {
    /// Construct a preset fastembed-rs EmbeddingGemma 300M embedder (768d).
    ///
    /// - `cache_dir`: filesystem path for the auto-downloaded ONNX weights.
    ///   `None` defers to the `$LUNARIS_FASTEMBED_CACHE_DIR` /
    ///   `~/.cache/lunaris/models/fastembed/` resolution chain.
    /// - `execution`: ORT execution provider. One of `"cpu"` / `"coreml"` /
    ///   `"cuda"`. Unknown values raise `ValueError`.
    /// - `show_download_progress`: emit fastembed's progress bar to stderr.
    ///   Default `false` keeps server logs clean.
    ///
    /// Raises `LunarisError` if the ONNX session fails to construct (e.g.
    /// HF Hub download error, corrupted cache).
    #[staticmethod]
    #[pyo3(signature = (cache_dir=None, execution="cpu", show_download_progress=false))]
    fn fastembed(
        cache_dir: Option<PathBuf>,
        execution: &str,
        show_download_progress: bool,
    ) -> PyResult<Self> {
        let exec = parse_execution(execution)?;
        let opts = FastembedEmbedderOpts { cache_dir, show_download_progress, execution: exec };
        let embedder = FastembedEmbedder::new(opts).map_err(py_err)?;
        let dim = embedder.dim();
        Ok(Self { inner: Arc::new(embedder), backend: "fastembed", dim })
    }

    /// Bring-your-own ONNX model from in-memory bytes.
    ///
    /// Use this when the operator has the model bytes in memory (e.g.
    /// fetched from S3 or a model registry). For on-disk models, prefer
    /// [`EmbedderConfig::from_onnx_path`] which reads the bytes for you.
    ///
    /// - `onnx_bytes`: raw `model.onnx` bytes.
    /// - `tokenizer_bytes`: raw HF-format `tokenizer.json` bytes.
    /// - `dim`: declared output dimensionality (MUST match the ONNX graph).
    ///   A mismatch surfaces on first `embed_batch` as a `LunarisError`.
    /// - `pooling`: `"mean"` (default, sentence-similarity) or `"cls"`
    ///   (BERT-style). Unknown values raise `ValueError`.
    /// - `tokenizer_config_bytes` / `special_tokens_map_bytes` /
    ///   `config_bytes`: optional HF auxiliary files.
    /// - `execution`: ORT execution provider (`"cpu"` / `"coreml"` /
    ///   `"cuda"`).
    ///
    /// # Trust
    ///
    /// The ONNX bytes execute in-process through ONNX Runtime. Operators
    /// MUST source them from a trusted registry — Lunaris performs no
    /// graph validation. See `T-21-01-01` in the Phase 21-01 threat model.
    #[staticmethod]
    #[pyo3(signature = (
        onnx_bytes,
        tokenizer_bytes,
        dim,
        pooling = "mean",
        tokenizer_config_bytes = None,
        special_tokens_map_bytes = None,
        config_bytes = None,
        execution = "cpu",
    ))]
    #[allow(clippy::too_many_arguments)]
    fn from_onnx_bytes(
        onnx_bytes: Vec<u8>,
        tokenizer_bytes: Vec<u8>,
        dim: usize,
        pooling: &str,
        tokenizer_config_bytes: Option<Vec<u8>>,
        special_tokens_map_bytes: Option<Vec<u8>>,
        config_bytes: Option<Vec<u8>>,
        execution: &str,
    ) -> PyResult<Self> {
        let pooling = parse_pooling(pooling)?;
        let exec = parse_execution(execution)?;
        let opts = FastembedUserDefinedOpts {
            onnx_file: onnx_bytes,
            tokenizer_file: tokenizer_bytes,
            tokenizer_config_file: tokenizer_config_bytes,
            special_tokens_map_file: special_tokens_map_bytes,
            config_file: config_bytes,
            dim,
            pooling,
            execution: exec,
            max_length: FASTEMBED_GEMMA_MAX_TOKENS,
        };
        let embedder = FastembedEmbedder::from_user_defined(opts).map_err(py_err)?;
        // BYO models must validate dim on first call — see DimValidatingEmbedder.
        let validating = DimValidatingEmbedder::new(Arc::new(embedder), dim);
        Ok(Self { inner: Arc::new(validating), backend: "fastembed-user-defined", dim })
    }

    /// Bring-your-own ONNX model from on-disk paths.
    ///
    /// Reads the bytes from each path and delegates to
    /// [`EmbedderConfig::from_onnx_bytes`]. The optional `*_path` arguments
    /// resolve to `None` byte options if not provided. File-read failures
    /// raise `LunarisError` with the failing path in the message.
    #[staticmethod]
    #[pyo3(signature = (
        onnx_path,
        tokenizer_path,
        dim,
        pooling = "mean",
        tokenizer_config_path = None,
        special_tokens_map_path = None,
        config_path = None,
        execution = "cpu",
    ))]
    #[allow(clippy::too_many_arguments)]
    fn from_onnx_path(
        onnx_path: PathBuf,
        tokenizer_path: PathBuf,
        dim: usize,
        pooling: &str,
        tokenizer_config_path: Option<PathBuf>,
        special_tokens_map_path: Option<PathBuf>,
        config_path: Option<PathBuf>,
        execution: &str,
    ) -> PyResult<Self> {
        let onnx_bytes = read_path("onnx_path", &onnx_path)?;
        let tokenizer_bytes = read_path("tokenizer_path", &tokenizer_path)?;
        let tokenizer_config_bytes = tokenizer_config_path
            .as_deref()
            .map(|p| read_path("tokenizer_config_path", p))
            .transpose()?;
        let special_tokens_map_bytes = special_tokens_map_path
            .as_deref()
            .map(|p| read_path("special_tokens_map_path", p))
            .transpose()?;
        let config_bytes =
            config_path.as_deref().map(|p| read_path("config_path", p)).transpose()?;
        Self::from_onnx_bytes(
            onnx_bytes,
            tokenizer_bytes,
            dim,
            pooling,
            tokenizer_config_bytes,
            special_tokens_map_bytes,
            config_bytes,
            execution,
        )
    }

    /// HTTP-backed Ollama embedder.
    ///
    /// - `endpoint`: Ollama base URL. `None` falls back to
    ///   `$LUNARIS_OLLAMA_URL` then `http://localhost:11434`.
    /// - `model`: Ollama model tag. `None` falls back to
    ///   `$LUNARIS_OLLAMA_MODEL` then `embeddinggemma:300m`.
    /// - `dim`: expected output dim (default 768). The Ollama backend
    ///   rejects rows whose length differs from this value.
    ///
    /// The constructor only builds a `reqwest::Client` — it does NOT
    /// contact the Ollama server. Network errors surface on the first
    /// `embed_batch` call.
    #[staticmethod]
    #[pyo3(signature = (endpoint=None, model=None, dim=768))]
    fn ollama(endpoint: Option<String>, model: Option<String>, dim: usize) -> PyResult<Self> {
        let defaults = OllamaEmbedderOpts::default();
        let opts = OllamaEmbedderOpts {
            endpoint: endpoint.unwrap_or(defaults.endpoint),
            model: model.unwrap_or(defaults.model),
            dim,
        };
        let embedder = OllamaEmbedder::new(opts).map_err(py_err)?;
        Ok(Self { inner: Arc::new(embedder), backend: "ollama", dim })
    }

    /// Debug-friendly repr: `EmbedderConfig(backend='fastembed', dim=768)`.
    fn __repr__(&self) -> String {
        format!("EmbedderConfig(backend={:?}, dim={})", self.backend, self.dim)
    }

    /// Reported dim of the wrapped backend. For BYO factories this is the
    /// caller's declared dim (validated on first `embed_batch`).
    #[getter]
    fn dim(&self) -> usize {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the SDK-facing execution preference string into the typed enum.
///
/// Accepts `"cpu"`, `"coreml"`, `"cuda"` (lower-case). Unlike the
/// `lunaris_embed::fastembed::parse_execution` upstream helper — which
/// silently coerces unknown values to `Cpu` with a `tracing::warn` — this
/// strict variant rejects unknown values with a `PyValueError` so Python
/// callers see a typo immediately.
fn parse_execution(s: &str) -> PyResult<ExecutionPreference> {
    match s {
        "cpu" => Ok(ExecutionPreference::Cpu),
        #[cfg(feature = "fastembed-coreml")]
        "coreml" => Ok(ExecutionPreference::CoreMlThenCpu),
        #[cfg(feature = "fastembed-cuda")]
        "cuda" => Ok(ExecutionPreference::CudaThenCpu),
        // The `coreml` / `cuda` variants are gated behind the matching
        // `lunaris-embed` cargo features; without them, fastembed runs CPU-only.
        // We surface a clear PyValueError so the operator knows to rebuild the
        // wheel with the relevant feature rather than getting a silent CPU
        // fallback at runtime.
        "coreml" => Err(PyValueError::new_err(
            "EmbedderConfig: execution=\"coreml\" requires the lunaris-py wheel \
             to be built with the lunaris-embed/fastembed-coreml feature",
        )),
        "cuda" => Err(PyValueError::new_err(
            "EmbedderConfig: execution=\"cuda\" requires the lunaris-py wheel \
             to be built with the lunaris-embed/fastembed-cuda feature",
        )),
        other => Err(PyValueError::new_err(format!(
            "EmbedderConfig: unknown execution {other:?} — expected one of \
             \"cpu\", \"coreml\", \"cuda\""
        ))),
    }
}

/// Parse the SDK-facing pooling-mode string into the typed enum.
///
/// Accepts `"mean"` (default for sentence-similarity) and `"cls"`
/// (BERT-style first-token). Unknown values — including `"none"` which the
/// upstream `PoolingMode` enum does NOT support — are rejected with
/// `PyValueError`.
fn parse_pooling(s: &str) -> PyResult<PoolingMode> {
    match s {
        "mean" => Ok(PoolingMode::Mean),
        "cls" => Ok(PoolingMode::Cls),
        other => Err(PyValueError::new_err(format!(
            "EmbedderConfig: unknown pooling {other:?} — expected \"mean\" or \"cls\""
        ))),
    }
}

/// Read `path` into a `Vec<u8>`. On failure produce a `LunarisError`-mapped
/// `PyErr` whose message names the field (so the operator can tell which
/// of the four optional file paths blew up).
fn read_path(field: &str, path: &std::path::Path) -> PyResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        py_err(LunarisError::Storage(StorageError::Backend(format!(
            "EmbedderConfig.from_onnx_path: failed to read {field}={}: {e}",
            path.display()
        ))))
    })
}

// ---------------------------------------------------------------------------
// DimValidatingEmbedder — BYO dim safety wrapper
// ---------------------------------------------------------------------------

/// Wraps a BYO embedder and asserts that the first emitted vector matches
/// the operator-declared dim. Subsequent calls pass through unchanged
/// (one-shot validation; the ONNX graph shape doesn't drift call-to-call).
///
/// Implements [`Embedder`] so it can replace the inner embedder transparently.
/// Used by [`EmbedderConfig::from_onnx_bytes`] and (transitively)
/// [`EmbedderConfig::from_onnx_path`]. The preset `fastembed()` and
/// `ollama()` paths skip the wrapper — their dim is known from constants.
struct DimValidatingEmbedder {
    inner: Arc<dyn Embedder>,
    declared_dim: usize,
    validated: AtomicBool,
}

impl DimValidatingEmbedder {
    fn new(inner: Arc<dyn Embedder>, declared_dim: usize) -> Self {
        Self { inner, declared_dim, validated: AtomicBool::new(false) }
    }
}

#[async_trait]
impl Embedder for DimValidatingEmbedder {
    fn dim(&self) -> usize {
        self.declared_dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        let out = self.inner.embed_batch(inputs).await?;
        // One-shot validation: only the first non-empty batch runs the check.
        // `swap` returns the previous value — `false` means we haven't
        // validated yet. Relaxed ordering is fine: the check is idempotent
        // (same dim every call) and the only contract is "fire at least once".
        if !self.validated.swap(true, Ordering::Relaxed)
            && let Some(first) = out.first()
            && first.len() != self.declared_dim
        {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "EmbedderConfig.from_onnx_*: declared dim {} does not match \
                 observed dim {} — check the ONNX model output shape",
                self.declared_dim,
                first.len()
            ))));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Free function: lunaris_with_embedder — exposed in the pymodule
// ---------------------------------------------------------------------------

/// Apply an [`EmbedderConfig`] to a freshly-constructed `Lunaris` handle.
///
/// Implementation detail: PyO3 0.26 does NOT support multiple
/// `#[pymethods]` blocks on the same `#[pyclass]` without the
/// `multiple-pymethods` feature, which Lunaris does not enable. The free
/// function pattern mirrors `scope.rs::lunaris_scoped` — wired up as a
/// method on the `Lunaris` Python class via `python/lunaris/__init__.py`.
#[pyfunction]
#[pyo3(signature = (handle, cfg))]
pub(crate) fn lunaris_with_embedder(
    handle: PyRef<'_, crate::generated::PyLunaris>,
    cfg: PyRef<'_, EmbedderConfig>,
) -> PyResult<crate::generated::PyLunaris> {
    // `Lunaris::with_embedder` consumes-and-returns. Our `PyLunaris` holds
    // an `Arc<Lunaris>` — clone the inner via `(*handle.inner).clone()` (the
    // Rust handle is `Clone`), apply the swap, and rebox into a fresh Arc.
    let new_handle = (*handle.inner).clone().with_embedder(cfg.inner.clone());
    Ok(crate::generated::PyLunaris { inner: Arc::new(new_handle) })
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EmbedderConfig>()?;
    m.add_function(wrap_pyfunction!(lunaris_with_embedder, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_execution_cpu() {
        assert!(matches!(parse_execution("cpu").unwrap(), ExecutionPreference::Cpu));
    }

    #[cfg(feature = "fastembed-coreml")]
    #[test]
    fn parse_execution_coreml() {
        assert!(matches!(parse_execution("coreml").unwrap(), ExecutionPreference::CoreMlThenCpu));
    }

    #[cfg(feature = "fastembed-cuda")]
    #[test]
    fn parse_execution_cuda() {
        assert!(matches!(parse_execution("cuda").unwrap(), ExecutionPreference::CudaThenCpu));
    }

    /// Without the `fastembed-coreml` / `fastembed-cuda` features, asking
    /// for those accelerators surfaces an actionable PyValueError.
    #[cfg(not(feature = "fastembed-coreml"))]
    #[test]
    fn parse_execution_coreml_without_feature() {
        let err = parse_execution("coreml").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("fastembed-coreml"), "msg: {msg}");
    }

    #[cfg(not(feature = "fastembed-cuda"))]
    #[test]
    fn parse_execution_cuda_without_feature() {
        let err = parse_execution("cuda").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("fastembed-cuda"), "msg: {msg}");
    }

    #[test]
    fn parse_execution_unknown_carries_bad_value() {
        let err = parse_execution("bogus").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bogus"), "error message must echo bad value: {msg}");
        assert!(msg.contains("execution"), "error message must name field: {msg}");
    }

    #[test]
    fn parse_pooling_known() {
        assert_eq!(parse_pooling("mean").unwrap(), PoolingMode::Mean);
        assert_eq!(parse_pooling("cls").unwrap(), PoolingMode::Cls);
    }

    #[test]
    fn parse_pooling_unknown() {
        let err = parse_pooling("none").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("none"), "error message must echo bad value: {msg}");
        assert!(msg.contains("pooling"), "error message must name field: {msg}");
    }

    /// DimValidatingEmbedder fires on the first call when dims mismatch.
    #[tokio::test]
    async fn dim_validating_embedder_rejects_mismatch_on_first_call() {
        use lunaris_core::StubEmbedder;
        let inner: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
        let validator = DimValidatingEmbedder::new(inner, 1024); // declare wrong
        let err = validator.embed_batch(&["x"]).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("observed dim"), "msg: {msg}");
        assert!(msg.contains("1024"), "msg: {msg}");
        assert!(msg.contains("768"), "msg: {msg}");
    }

    #[tokio::test]
    async fn dim_validating_embedder_accepts_match() {
        use lunaris_core::StubEmbedder;
        let inner: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
        let validator = DimValidatingEmbedder::new(inner, 768);
        let out = validator.embed_batch(&["x"]).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 768);
        // Second call still passes — validation is one-shot.
        let out2 = validator.embed_batch(&["y"]).await.unwrap();
        assert_eq!(out2[0].len(), 768);
    }

    #[test]
    fn read_path_missing_file_carries_path() {
        let err = read_path("onnx_path", std::path::Path::new("/nonexistent/x.onnx")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("onnx_path"), "msg: {msg}");
        assert!(msg.contains("/nonexistent/x.onnx"), "msg: {msg}");
    }
}
