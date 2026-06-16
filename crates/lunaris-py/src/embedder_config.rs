//! v0.4 N-03 cutover — `EmbedderConfig` for the Python SDK, rewritten on
//! `lunaris-embed-native::NativeEmbedder` (+ `NativeQuantizedEmbedder` under
//! `embedder-gguf`). The retired `EmbedderConfig.fastembed()` /
//! `EmbedderConfig.from_onnx_path()` / `EmbedderConfig.ollama()` factories
//! are replaced by `EmbedderConfig.native()` and
//! `EmbedderConfig.native_quantized()` — see
//! `docs/migration/0.3-to-0.4-native-default.md` for the Python migration
//! recipe.
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
//!
//! ## Factory methods
//!
//! - [`EmbedderConfig::native`] — FP16 candle granite-r2 embedder from a
//!   model directory (default `<cache>/lunaris/models/granite-embedding-311m-multilingual-r2/`).
//! - [`EmbedderConfig::native_quantized`] — Q4_K_M GGUF embedder (gated
//!   on the `lunaris-embed-native/embedder-gguf` feature at wheel-build time).
//! - [`EmbedderConfig::noop`] — zero-vector backend at caller-chosen dim.
//!
//! TODO(N-04): pre-existing Scope-codegen errors in `crates/lunaris-ts` and
//! `crates/lunaris-py` are tracked separately; this rewrite does not depend
//! on the codegen output and is unaffected.

use std::path::PathBuf;
use std::sync::Arc;

use lunaris_core::{Embedder, LunarisError, NOOP_DEFAULT_DIM, NoopEmbedder, StorageError};
use lunaris_embed_native::{GRANITE_R2_DIM, NativeEmbedder, NativeEmbedderOpts};
#[cfg(feature = "embedder-gguf")]
use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::errors::py_err;

/// Opaque holder for a resolved [`Embedder`] backend.
///
/// Construct via the [`EmbedderConfig::native`] /
/// [`EmbedderConfig::native_quantized`] / [`EmbedderConfig::noop`] static
/// methods, then pass to `Lunaris.open(url, embedder=cfg)`.
///
/// ```python
/// from lunaris import EmbedderConfig, open
///
/// cfg = EmbedderConfig.native()  # uses default cache dir
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
    /// Construct the FP16 candle `NativeEmbedder` backed by
    /// `ibm-granite/granite-embedding-311m-multilingual-r2` (768-d).
    ///
    /// - `model_dir`: directory holding `model.safetensors`, `tokenizer.json`,
    ///   `config.json`. `None` defers to the canonical cache layout
    ///   `<cache_dir>/lunaris/models/granite-embedding-311m-multilingual-r2/`.
    ///
    /// Raises `LunarisError` if any of the three artifacts is missing.
    #[staticmethod]
    #[pyo3(signature = (model_dir=None))]
    fn native(model_dir: Option<PathBuf>) -> PyResult<Self> {
        let dir = model_dir.unwrap_or_else(default_granite_dir);
        let opts = NativeEmbedderOpts {
            weights_path: dir.join("model.safetensors"),
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let e = NativeEmbedder::open(opts).map_err(|e| py_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(e), backend: "native", dim: GRANITE_R2_DIM })
    }

    /// Construct the Q4_K_M GGUF `NativeQuantizedEmbedder`. Requires the
    /// wheel to be built with the `embedder-gguf` feature.
    ///
    /// - `gguf_path`: path to the granite-r2 Q4_K_M GGUF artifact.
    /// - `model_dir`: directory holding `tokenizer.json` + `config.json`
    ///   (the GGUF carries weights but not the tokenizer/config). `None`
    ///   uses the canonical cache layout.
    #[staticmethod]
    #[pyo3(signature = (gguf_path, model_dir=None))]
    #[cfg(feature = "embedder-gguf")]
    fn native_quantized(gguf_path: PathBuf, model_dir: Option<PathBuf>) -> PyResult<Self> {
        let dir = model_dir.unwrap_or_else(default_granite_dir);
        let opts = NativeQuantizedEmbedderOpts {
            gguf_path,
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let e = NativeQuantizedEmbedder::open(opts).map_err(|e| py_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(e), backend: "native-quantized", dim: GRANITE_R2_DIM })
    }

    /// Stub raising `ValueError` when the wheel was built without the
    /// `embedder-gguf` feature. Surfaces a clear error rather than a
    /// silent `AttributeError`.
    #[staticmethod]
    #[pyo3(signature = (_gguf_path=None, _model_dir=None))]
    #[cfg(not(feature = "embedder-gguf"))]
    #[allow(unused_variables)]
    fn native_quantized(
        _gguf_path: Option<PathBuf>,
        _model_dir: Option<PathBuf>,
    ) -> PyResult<Self> {
        Err(PyValueError::new_err(
            "EmbedderConfig.native_quantized() requires the lunaris-py wheel to be built \
             with the `embedder-gguf` feature (forwards to \
             lunaris-embed-native/embedder-gguf).",
        ))
    }

    /// Zero-vector embedder for metadata-only ingest / BYO-vector flows /
    /// unit tests. Returns all-zero vectors of length `dim`.
    ///
    /// `dim` defaults to [`NOOP_DEFAULT_DIM`] = 768 — matching granite-r2 —
    /// so storage indices stay interoperable across noop ↔ native swaps.
    #[staticmethod]
    #[pyo3(signature = (dim = NOOP_DEFAULT_DIM))]
    fn noop(dim: usize) -> Self {
        Self { inner: Arc::new(NoopEmbedder::new(dim)), backend: "noop", dim }
    }

    /// Debug-friendly repr: `EmbedderConfig(backend='native', dim=768)`.
    fn __repr__(&self) -> String {
        format!("EmbedderConfig(backend={:?}, dim={})", self.backend, self.dim)
    }

    /// Reported dim of the wrapped backend.
    #[getter]
    fn dim(&self) -> usize {
        self.dim
    }
}

/// Canonical cache directory for the granite-r2 model artifacts. Mirrors
/// `lunaris::handle::default_model_dir(GRANITE_R2_DIR)` so the SDK + the
/// umbrella agree on the on-disk layout.
fn default_granite_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lunaris").join("models").join("granite-embedding-311m-multilingual-r2")
}

/// Apply an [`EmbedderConfig`] to a freshly-constructed `Lunaris` handle.
/// PyO3 0.26 does not support multiple `#[pymethods]` blocks without the
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
/// potential BYO-weight callers in N-04.
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
        assert_eq!(cfg.dim(), GRANITE_R2_DIM);
    }

    #[test]
    fn default_granite_dir_ends_with_canonical_name() {
        let p = default_granite_dir();
        assert!(
            p.ends_with("lunaris/models/granite-embedding-311m-multilingual-r2"),
            "got {}",
            p.display()
        );
    }
}
