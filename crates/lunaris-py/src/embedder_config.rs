//! llama.cpp-only cutover (2026-07, Phase C) — `EmbedderConfig` for the
//! Python SDK, rewritten on `lunaris-llamacpp::LlamaCppEmbedder` (granite-r2
//! Q4_K_M GGUF). The retired v0.4 `EmbedderConfig.native()` /
//! `EmbedderConfig.native_quantized()` factories (candle) fail loudly with a
//! migration hint; the supported factories are [`EmbedderConfig::llamacpp`]
//! and [`EmbedderConfig::noop`].
//!
//! This module is **NOT codegen-managed** (mirrors the `scope.rs` /
//! `toggles.rs` precedent). `EmbedderConfig` is an opaque holder around a
//! resolved `Arc<dyn Embedder>` that the Python SDK passes back into
//! `Lunaris.open(url, embedder=cfg)`.
//!
//! ## FFI cliff
//!
//! Python callers CANNOT implement the [`Embedder`] trait directly — that
//! would require per-call Python→Rust callbacks, which are slow and brittle.
//! The factory methods below cover every customization the Rust crate
//! supports MINUS the "roll your own trait impl" escape hatch (Rust-only).

use std::path::PathBuf;
use std::sync::Arc;

use lunaris_core::{Embedder, LunarisError, NOOP_DEFAULT_DIM, NoopEmbedder, StorageError};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::errors::py_err;

/// Opaque holder for a resolved [`Embedder`] backend.
///
/// Construct via the [`EmbedderConfig::llamacpp`] / [`EmbedderConfig::noop`]
/// static methods, then pass to `Lunaris.open(url, embedder=cfg)`.
///
/// ```python
/// from lunaris import EmbedderConfig, open
///
/// cfg = EmbedderConfig.llamacpp()  # uses the ~/.lunaris/models/ staged GGUF
/// handle = await open(url, embedder=cfg)
/// ```
#[pyclass(frozen, name = "EmbedderConfig", module = "lunaris", from_py_object)]
#[derive(Clone)]
pub struct EmbedderConfig {
    pub(crate) inner: Arc<dyn Embedder>,
    pub(crate) backend: &'static str,
    pub(crate) dim: usize,
}

#[pymethods]
impl EmbedderConfig {
    /// Construct the llama.cpp `LlamaCppEmbedder` backed by the granite-r2
    /// Q4_K_M GGUF (768-d).
    ///
    /// - `gguf_path`: path to the GGUF artifact. `None` defers to the staged
    ///   default `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`.
    ///
    /// Raises `LunarisError` if the artifact is missing or fails to load.
    #[staticmethod]
    #[pyo3(signature = (gguf_path=None))]
    #[cfg(feature = "llamacpp")]
    fn llamacpp(gguf_path: Option<PathBuf>) -> PyResult<Self> {
        let gguf_path = gguf_path.unwrap_or_else(default_embedder_gguf);
        let opts = lunaris_llamacpp::LlamaCppEmbedderOpts { gguf_path, ..Default::default() };
        let e = lunaris_llamacpp::LlamaCppEmbedder::open(opts)
            .map_err(|e| py_err(lunaris_core::LunarisError::from(e)))?;
        let dim = e.dim();
        Ok(Self { inner: Arc::new(e), backend: "llamacpp", dim })
    }

    /// Stub raising `ValueError` when the wheel was built without the
    /// `llamacpp` feature (Tier-0 build). Surfaces a clear error rather
    /// than a silent `AttributeError`.
    #[staticmethod]
    #[pyo3(signature = (_gguf_path=None))]
    #[cfg(not(feature = "llamacpp"))]
    #[allow(unused_variables)]
    fn llamacpp(_gguf_path: Option<PathBuf>) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "EmbedderConfig.llamacpp() requires the lunaris wheel to be built with the \
             `llamacpp` feature (this is a Tier-0 no-inference build). Use \
             EmbedderConfig.noop(), a remote embedder, or install a full wheel.",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle `NativeEmbedder` was
    /// deleted. Use [`EmbedderConfig::llamacpp`] instead.
    #[staticmethod]
    #[pyo3(signature = (_model_dir=None))]
    #[allow(unused_variables)]
    fn native(_model_dir: Option<PathBuf>) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "EmbedderConfig.native() was removed in the llama.cpp-only cutover (v0.6): \
             the candle FP16 embedder no longer exists. Use EmbedderConfig.llamacpp() \
             (granite-r2 Q4_K_M GGUF, same 768-d vectors).",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle quantized embedder was
    /// deleted. Use [`EmbedderConfig::llamacpp`] instead.
    #[staticmethod]
    #[pyo3(signature = (_gguf_path=None, _model_dir=None))]
    #[allow(unused_variables)]
    fn native_quantized(
        _gguf_path: Option<PathBuf>,
        _model_dir: Option<PathBuf>,
    ) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "EmbedderConfig.native_quantized() was removed in the llama.cpp-only cutover \
             (v0.6). Use EmbedderConfig.llamacpp(gguf_path) — same GGUF artifact, served \
             by llama.cpp.",
        ))
    }

    /// Zero-vector embedder for metadata-only ingest / BYO-vector flows /
    /// unit tests. Returns all-zero vectors of length `dim`.
    ///
    /// `dim` defaults to [`NOOP_DEFAULT_DIM`] = 768 — matching granite-r2 —
    /// so storage indices stay interoperable across noop ↔ llamacpp swaps.
    #[staticmethod]
    #[pyo3(signature = (dim = NOOP_DEFAULT_DIM))]
    fn noop(dim: usize) -> Self {
        Self { inner: Arc::new(NoopEmbedder::new(dim)), backend: "noop", dim }
    }

    /// Debug-friendly repr: `EmbedderConfig(backend='llamacpp', dim=768)`.
    fn __repr__(&self) -> String {
        format!("EmbedderConfig(backend={:?}, dim={})", self.backend, self.dim)
    }

    /// Reported dim of the wrapped backend.
    #[getter]
    fn dim(&self) -> usize {
        self.dim
    }
}

/// Staged default location for the embedder GGUF. Mirrors the umbrella
/// `lunaris::handle` staged-artifact layout (`~/.lunaris/models/`) so the SDK
/// + the umbrella agree on the on-disk layout.
#[cfg(feature = "llamacpp")]
fn default_embedder_gguf() -> PathBuf {
    lunaris_models_dir().join("granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
}

/// `~/.lunaris/models/` — the GGUF staging directory shared with the
/// umbrella resolver and the `stage-models` tool.
#[cfg(feature = "llamacpp")]
pub(crate) fn lunaris_models_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lunaris")
        .join("models")
}

/// Apply an [`EmbedderConfig`] to a freshly-constructed `Lunaris` handle.
/// PyO3 0.29 does not support multiple `#[pymethods]` blocks without the
/// `multiple-pymethods` feature; the free-function pattern mirrors
/// `scope.rs::lunaris_scoped`.
#[pyfunction]
#[pyo3(signature = (handle, cfg))]
pub(crate) fn lunaris_with_embedder(
    handle: PyRef<'_, crate::generated::PyLunaris>,
    cfg: PyRef<'_, EmbedderConfig>,
) -> PyResult<crate::generated::PyLunaris> {
    let new_handle = (*handle.inner).clone().with_embedder(cfg.inner.clone());
    Ok(crate::generated::PyLunaris { inner: Arc::new(new_handle) })
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EmbedderConfig>()?;
    m.add_function(wrap_pyfunction!(lunaris_with_embedder, m)?)?;
    Ok(())
}

/// Read `path` into a `Vec<u8>`, surfacing the failing field name. Kept for
/// potential BYO-weight callers.
#[allow(dead_code)]
fn read_path(field: &str, path: &std::path::Path) -> PyResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        py_err(LunarisError::Storage(StorageError::Backend(format!(
            "EmbedderConfig: failed to read {field}={}: {e}",
            path.display()
        ))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_default_dim_matches_granite_r2() {
        let cfg = EmbedderConfig::noop(NOOP_DEFAULT_DIM);
        assert_eq!(cfg.dim(), 768);
    }

    #[test]
    fn retired_native_factories_fail_loudly() {
        // PyErr rendering needs the GIL; asserting Err is enough here — the
        // message content is a compile-time literal above.
        assert!(EmbedderConfig::native(None).is_err());
        assert!(EmbedderConfig::native_quantized(None, None).is_err());
    }

    #[cfg(feature = "llamacpp")]
    #[test]
    fn default_embedder_gguf_ends_with_staged_name() {
        let p = default_embedder_gguf();
        assert!(
            p.ends_with(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"),
            "got {}",
            p.display()
        );
    }
}
