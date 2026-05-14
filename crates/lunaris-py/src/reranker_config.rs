//! v0.4 N-03 cutover — `RerankerConfig` for the Python SDK, rewritten on
//! `lunaris-rerank-native::NativeReranker` (+ `NativeQuantizedReranker` under
//! `reranker-gguf`). The retired `RerankerConfig.fastembed()` factory is
//! replaced by `RerankerConfig.native()` and `::native_quantized()` — see
//! `docs/migration/0.3-to-0.4-native-default.md`.
//!
//! Not codegen-managed (mirrors `embedder_config.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use lunaris_core::LunarisError;
use lunaris_rerank::{NoopReranker, Reranker};
use lunaris_rerank_native::{NativeReranker, NativeRerankerOpts};
#[cfg(feature = "reranker-gguf")]
use lunaris_rerank_native::{NativeQuantizedReranker, NativeQuantizedRerankerOpts};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::errors::py_err;

/// Opaque holder for a resolved [`Reranker`] backend.
///
/// Construct via [`RerankerConfig::native`] / [`RerankerConfig::native_quantized`]
/// / [`RerankerConfig::noop`] then pass to `Lunaris.open(url, reranker=cfg)`.
#[pyclass(frozen, name = "RerankerConfig", module = "lunaris")]
#[derive(Clone)]
pub struct RerankerConfig {
    pub(crate) inner: Arc<dyn Reranker>,
    pub(crate) backend: &'static str,
}

#[pymethods]
impl RerankerConfig {
    /// FP32 candle `NativeReranker` backed by `BAAI/bge-reranker-v2-m3`.
    ///
    /// `model_dir` defaults to `<cache>/lunaris/models/bge-reranker-v2-m3/`.
    #[staticmethod]
    #[pyo3(signature = (model_dir=None))]
    fn native(model_dir: Option<PathBuf>) -> PyResult<Self> {
        let dir = model_dir.unwrap_or_else(default_bge_dir);
        let opts = NativeRerankerOpts {
            weights_path: dir.join("model.safetensors"),
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let r = NativeReranker::open(opts).map_err(|e| py_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(r), backend: "native" })
    }

    /// Q4_K_M GGUF `NativeQuantizedReranker` — requires the wheel built with
    /// `reranker-gguf`.
    #[staticmethod]
    #[pyo3(signature = (gguf_path, model_dir=None))]
    #[cfg(feature = "reranker-gguf")]
    fn native_quantized(gguf_path: PathBuf, model_dir: Option<PathBuf>) -> PyResult<Self> {
        let dir = model_dir.unwrap_or_else(default_bge_dir);
        let opts = NativeQuantizedRerankerOpts {
            gguf_path,
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let r =
            NativeQuantizedReranker::open(opts).map_err(|e| py_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(r), backend: "native-quantized" })
    }

    #[staticmethod]
    #[pyo3(signature = (_gguf_path=None, _model_dir=None))]
    #[cfg(not(feature = "reranker-gguf"))]
    #[allow(unused_variables)]
    fn native_quantized(_gguf_path: PathBuf, _model_dir: Option<PathBuf>) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "RerankerConfig.native_quantized() requires the lunaris-py wheel to be built \
             with the `reranker-gguf` feature.",
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

fn default_bge_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lunaris").join("models").join("bge-reranker-v2-m3")
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
    fn default_bge_dir_ends_with_canonical_name() {
        let p = default_bge_dir();
        assert!(p.ends_with("lunaris/models/bge-reranker-v2-m3"), "got {}", p.display());
    }
}
