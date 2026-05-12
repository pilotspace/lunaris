//! Phase 21 Plan 21-01 — handwritten `RerankerConfig` for the Python SDK.
//!
//! This module is **NOT codegen-managed** (mirrors `embedder_config.rs` and
//! the `scope.rs` / `toggles.rs` precedent). The Python SDK passes a
//! `RerankerConfig` back to `Lunaris.open(url, reranker=cfg)` to override the
//! env-driven default.
//!
//! ## FFI cliff
//!
//! Python callers CANNOT implement the [`Reranker`] trait directly — same
//! reasoning as `embedder_config.rs`. The factories below cover the two
//! presets that ship in `lunaris-rerank`.
//!
//! ## Factory methods
//!
//! - [`RerankerConfig::fastembed`] — preset ONNX-backed BGE-Reranker-v2-m3
//!   cross-encoder via fastembed-rs (Phase 19-04).
//! - [`RerankerConfig::noop`] — `NoopReranker` passthrough (RETRIEVE-06
//!   fallback; equivalent to "disable rerank").
//!
//! ## BYO ONNX deferral (Task 1 finding)
//!
//! `grep user_defined ~/.cargo/registry/src/.../fastembed-5.13.4/src/reranking/`
//! shows fastembed 5.13.4 DOES expose `try_new_from_user_defined`
//! (`impl.rs:106`) and `UserDefinedRerankingModel` (`init.rs:81`). However
//! `lunaris-rerank::FastembedReranker` does NOT yet wrap this constructor —
//! only `FastembedReranker::new` exists today (Phase 19-04). Adding the BYO
//! factory to the SDK requires first adding a `FastembedReranker::from_user_defined`
//! wrapper in `lunaris-rerank`, which is outside Phase 21-01's
//! `files_modified` scope. **Deferred** to a follow-up phase that updates
//! `lunaris-rerank` then this module in lockstep.

use std::path::PathBuf;
use std::sync::Arc;

use lunaris_rerank::Reranker;
use lunaris_rerank::fastembed::{FastembedReranker, FastembedRerankerOpts};
// `lunaris-rerank` re-exports its own `ExecutionPreference` (NOT the embed
// crate's — they are distinct types because the cfg-gated variants live behind
// per-crate feature flags). Use this one when constructing
// `FastembedRerankerOpts`.
use lunaris_rerank::fastembed_exec::ExecutionPreference;
use lunaris_rerank::noop::NoopReranker;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::errors::py_err;

// ---------------------------------------------------------------------------
// RerankerConfig
// ---------------------------------------------------------------------------

/// Opaque holder for a resolved [`Reranker`] backend.
///
/// Construct via the [`RerankerConfig::fastembed`] / [`RerankerConfig::noop`]
/// static methods, then pass to `Lunaris.open(url, reranker=cfg)`.
///
/// ```python
/// from lunaris import RerankerConfig, open
///
/// cfg = RerankerConfig.fastembed(execution="cpu")
/// handle = await open(url, reranker=cfg)
/// ```
#[pyclass(frozen, name = "RerankerConfig", module = "lunaris")]
#[derive(Clone)]
pub struct RerankerConfig {
    pub(crate) inner: Arc<dyn Reranker>,
    pub(crate) backend: &'static str,
}

#[pymethods]
impl RerankerConfig {
    /// Construct a preset fastembed-rs BGE-Reranker-v2-m3 cross-encoder.
    ///
    /// - `cache_dir`: filesystem path for the auto-downloaded ONNX weights.
    ///   `None` defers to the
    ///   `$LUNARIS_FASTEMBED_RERANKER_CACHE_DIR` /
    ///   `~/.cache/lunaris/models/fastembed-reranker/` resolution chain.
    /// - `execution`: ORT execution provider. One of `"cpu"` / `"coreml"` /
    ///   `"cuda"`. Unknown values raise `ValueError`.
    /// - `show_download_progress`: emit fastembed's progress bar to stderr.
    ///
    /// Raises `LunarisError` if the ONNX session fails to construct.
    #[staticmethod]
    #[pyo3(signature = (cache_dir=None, execution="cpu", show_download_progress=false))]
    fn fastembed(
        cache_dir: Option<PathBuf>,
        execution: &str,
        show_download_progress: bool,
    ) -> PyResult<Self> {
        let exec = parse_execution(execution)?;
        let opts = FastembedRerankerOpts { cache_dir, show_download_progress, execution: exec };
        let reranker = FastembedReranker::new(opts).map_err(py_err)?;
        Ok(Self { inner: Arc::new(reranker), backend: "fastembed" })
    }

    /// `NoopReranker` — passthrough that returns docs unchanged.
    ///
    /// Equivalent to disabling the rerank pass (RETRIEVE-06 fallback).
    /// `Hit.rerank_applied` will be `false` for results that flow through
    /// this reranker, so callers can tell they got the degraded path.
    #[staticmethod]
    fn noop() -> Self {
        Self { inner: Arc::new(NoopReranker), backend: "noop" }
    }

    /// Debug-friendly repr: `RerankerConfig(backend='fastembed')`.
    fn __repr__(&self) -> String {
        format!("RerankerConfig(backend={:?})", self.backend)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Strict variant of execution-preference parsing — same shape as the
/// helper in `embedder_config.rs`. Duplicated here (rather than extracted)
/// to keep each handwritten module independently auditable and to avoid an
/// extra `pub(crate)` cross-module dep just for two small parsers.
fn parse_execution(s: &str) -> PyResult<ExecutionPreference> {
    match s {
        "cpu" => Ok(ExecutionPreference::Cpu),
        #[cfg(feature = "fastembed-coreml")]
        "coreml" => Ok(ExecutionPreference::CoreMlThenCpu),
        #[cfg(feature = "fastembed-cuda")]
        "cuda" => Ok(ExecutionPreference::CudaThenCpu),
        // Matches the embedder side — feature-gated variants require a wheel
        // rebuild; surface the requirement instead of a silent CPU fallback.
        "coreml" => Err(PyValueError::new_err(
            "RerankerConfig: execution=\"coreml\" requires the lunaris-py wheel \
             to be built with the lunaris-rerank/fastembed-coreml feature",
        )),
        "cuda" => Err(PyValueError::new_err(
            "RerankerConfig: execution=\"cuda\" requires the lunaris-py wheel \
             to be built with the lunaris-rerank/fastembed-cuda feature",
        )),
        other => Err(PyValueError::new_err(format!(
            "RerankerConfig: unknown execution {other:?} — expected one of \
             \"cpu\", \"coreml\", \"cuda\""
        ))),
    }
}

// ---------------------------------------------------------------------------
// Free function: lunaris_with_reranker — exposed in the pymodule
// ---------------------------------------------------------------------------

/// Apply a [`RerankerConfig`] to a freshly-constructed `Lunaris` handle.
///
/// Mirrors `lunaris_with_embedder` from `embedder_config.rs`. See that
/// module for the PyO3-multiple-pymethods rationale.
#[pyfunction]
#[pyo3(signature = (handle, cfg))]
pub(crate) fn lunaris_with_reranker(
    handle: PyRef<'_, crate::generated::PyLunaris>,
    cfg: PyRef<'_, RerankerConfig>,
) -> PyResult<crate::generated::PyLunaris> {
    let new_handle = (*handle.inner).clone().with_reranker(cfg.inner.clone());
    Ok(crate::generated::PyLunaris { inner: Arc::new(new_handle) })
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<RerankerConfig>()?;
    m.add_function(wrap_pyfunction!(lunaris_with_reranker, m)?)?;
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
        assert!(msg.contains("bogus"), "msg: {msg}");
        assert!(msg.contains("execution"), "msg: {msg}");
    }

    #[test]
    fn noop_factory_is_passthrough() {
        let cfg = RerankerConfig::noop();
        assert_eq!(cfg.backend, "noop");
        assert!(!cfg.inner.applies());
    }
}
