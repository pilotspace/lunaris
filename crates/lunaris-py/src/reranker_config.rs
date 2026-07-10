//! llama.cpp-only cutover (2026-07, Phase C) — `RerankerConfig` for the
//! Python SDK, rewritten on `lunaris-llamacpp::LlamaCppReranker`
//! (bge-reranker-v2-m3 Q5_K_M GGUF). The retired v0.4
//! `RerankerConfig.native()` / `::native_quantized()` factories (candle)
//! fail loudly with a migration hint; the supported factories are
//! [`RerankerConfig::llamacpp`] and [`RerankerConfig::noop`].
//!
//! Not codegen-managed (mirrors `embedder_config.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use lunaris_rerank::{NoopReranker, Reranker};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

#[cfg(feature = "llamacpp")]
use crate::errors::py_err;

/// Opaque holder for a resolved [`Reranker`] backend.
///
/// Construct via [`RerankerConfig::llamacpp`] / [`RerankerConfig::noop`]
/// then pass to `Lunaris.open(url, reranker=cfg)`.
#[pyclass(frozen, name = "RerankerConfig", module = "lunaris", from_py_object)]
#[derive(Clone)]
pub struct RerankerConfig {
    pub(crate) inner: Arc<dyn Reranker>,
    pub(crate) backend: &'static str,
}

#[pymethods]
impl RerankerConfig {
    /// llama.cpp `LlamaCppReranker` backed by the bge-reranker-v2-m3 Q5_K_M
    /// GGUF (cross-encoder, sigmoid scores ∈ [0, 1]).
    ///
    /// - `gguf_path`: path to the GGUF artifact. `None` defers to the staged
    ///   default `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf`.
    ///
    /// Loads eagerly and raises `LunarisError` on a missing/corrupt artifact
    /// (explicit construction = fail fast; the umbrella's default resolution
    /// defers the load to first rerank instead).
    #[staticmethod]
    #[pyo3(signature = (gguf_path=None))]
    #[cfg(feature = "llamacpp")]
    fn llamacpp(gguf_path: Option<PathBuf>) -> PyResult<Self> {
        let gguf_path = gguf_path.unwrap_or_else(default_reranker_gguf);
        let opts = lunaris_llamacpp::LlamaCppRerankerOpts { gguf_path, ..Default::default() };
        let r = lunaris_llamacpp::LlamaCppReranker::open(opts)
            .map_err(|e| py_err(lunaris_core::LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(r), backend: "llamacpp" })
    }

    /// Stub raising `ValueError` when the wheel was built without the
    /// `llamacpp` feature (Tier-0 build).
    #[staticmethod]
    #[pyo3(signature = (_gguf_path=None))]
    #[cfg(not(feature = "llamacpp"))]
    #[allow(unused_variables)]
    fn llamacpp(_gguf_path: Option<PathBuf>) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "RerankerConfig.llamacpp() requires the lunaris wheel to be built with the \
             `llamacpp` feature (this is a Tier-0 no-inference build). Use \
             RerankerConfig.noop() or install a full wheel.",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle `NativeReranker` was
    /// deleted. Use [`RerankerConfig::llamacpp`] instead.
    #[staticmethod]
    #[pyo3(signature = (_model_dir=None))]
    #[allow(unused_variables)]
    fn native(_model_dir: Option<PathBuf>) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "RerankerConfig.native() was removed in the llama.cpp-only cutover (v0.6): \
             the candle FP32 reranker no longer exists. Use RerankerConfig.llamacpp() \
             (bge-reranker-v2-m3 Q5_K_M GGUF, same sigmoid score contract).",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle quantized reranker was
    /// deleted. Use [`RerankerConfig::llamacpp`] instead.
    #[staticmethod]
    #[pyo3(signature = (_gguf_path=None, _model_dir=None))]
    #[allow(unused_variables)]
    fn native_quantized(
        _gguf_path: Option<PathBuf>,
        _model_dir: Option<PathBuf>,
    ) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "RerankerConfig.native_quantized() was removed in the llama.cpp-only cutover \
             (v0.6). Use RerankerConfig.llamacpp(gguf_path) — same GGUF artifact, served \
             by llama.cpp.",
        ))
    }

    /// `NoopReranker` — passthrough (RETRIEVE-06 fallback).
    #[staticmethod]
    fn noop() -> Self {
        Self { inner: Arc::new(NoopReranker), backend: "noop" }
    }

    fn __repr__(&self) -> String {
        format!("RerankerConfig(backend={:?})", self.backend)
    }
}

/// Staged default location for the reranker GGUF (`~/.lunaris/models/`).
#[cfg(feature = "llamacpp")]
fn default_reranker_gguf() -> PathBuf {
    crate::embedder_config::lunaris_models_dir().join("bge-reranker-v2-m3.Q5_K_M.gguf")
}

/// Apply a [`RerankerConfig`] to a freshly-constructed `Lunaris` handle.
#[pyfunction]
#[pyo3(signature = (handle, cfg))]
pub(crate) fn lunaris_with_reranker(
    handle: PyRef<'_, crate::generated::PyLunaris>,
    cfg: PyRef<'_, RerankerConfig>,
) -> PyResult<crate::generated::PyLunaris> {
    let new_handle = (*handle.inner).clone().with_reranker(cfg.inner.clone());
    Ok(crate::generated::PyLunaris { inner: Arc::new(new_handle) })
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<RerankerConfig>()?;
    m.add_function(wrap_pyfunction!(lunaris_with_reranker, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_factory_is_passthrough() {
        let cfg = RerankerConfig::noop();
        assert_eq!(cfg.backend, "noop");
        assert!(!cfg.inner.applies());
    }

    #[test]
    fn retired_native_factories_fail_loudly() {
        // PyErr rendering needs the GIL; asserting Err is enough — the
        // message content is a compile-time literal above.
        assert!(RerankerConfig::native(None).is_err());
        assert!(RerankerConfig::native_quantized(None, None).is_err());
    }

    #[cfg(feature = "llamacpp")]
    #[test]
    fn default_reranker_gguf_ends_with_staged_name() {
        let p = default_reranker_gguf();
        assert!(
            p.ends_with(".lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf"),
            "got {}",
            p.display()
        );
    }
}
