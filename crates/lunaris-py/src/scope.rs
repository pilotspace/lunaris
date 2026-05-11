//! Wave 3G — handwritten PyO3 bindings for the v0.2 multi-agent partitioning
//! surface: `Scope`, `EpisodeBuilder`, `ScopedLunaris`.
//!
//! These are NOT codegen-managed. The codegen IR cannot model:
//!
//! - `Scope`: a validating newtype whose constructor returns `Result`
//! - `EpisodeBuilder`: a consuming builder where the terminal `.into_episode`
//!   is `pub(crate)` (only `ScopedLunaris::ingest` may call it)
//! - `ScopedLunaris<'a>`: a borrowed view — PyO3 `#[pyclass]` requires
//!   `'static`, so the wrapper owns `Arc<Lunaris> + Scope` and re-derives the
//!   borrow on every method call.
//!
//! ## GIL discipline (CLAUDE.md)
//!
//! Every `.await` sits inside `pyo3_async_runtimes::tokio::future_into_py`
//! per the project's CLAUDE.md invariant.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};

use crate::errors::py_err;
use crate::generated::PyLunaris;

// ---------------------------------------------------------------------------
// PyScope
// ---------------------------------------------------------------------------

/// Multi-agent / multi-tenant partition key (RFC 0001).
///
/// Validates against `^[A-Za-z0-9_\\-:.]{1,128}$`. Raises `ValueError` on
/// construction if the string is empty, > 128 chars, or contains a character
/// outside the allowed set.
///
/// ```python
/// from lunaris import Scope
///
/// s = Scope("acme:agent-42")      # OK
/// s2 = Scope("")                  # raises ValueError
/// s3 = Scope("a" * 129)           # raises ValueError
/// ```
#[pyclass(name = "Scope", dict)]
#[derive(Clone)]
pub struct PyScope {
    pub(crate) inner: ::lunaris::Scope,
}

#[pymethods]
impl PyScope {
    /// Construct a `Scope` from `s`.
    ///
    /// Validates the regex `^[A-Za-z0-9_\\-:.]{1,128}$`. Raises `ValueError`
    /// if the string is empty, longer than 128 bytes, or contains a character
    /// outside the allowed set.
    #[new]
    fn new(s: &str) -> PyResult<Self> {
        let inner = ::lunaris::Scope::new(s)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Scope: {e}")))?;
        Ok(Self { inner })
    }

    /// Return the scope as a plain string.
    fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    fn __repr__(&self) -> String {
        format!("Scope({:?})", self.inner.as_str())
    }

    fn __str__(&self) -> &str {
        self.inner.as_str()
    }

    fn __eq__(&self, other: &PyScope) -> bool {
        self.inner == other.inner
    }
}

// ---------------------------------------------------------------------------
// PyEpisodeBuilder
// ---------------------------------------------------------------------------

/// Scope-less payload builder for an [`Episode`].
///
/// Assemble all Episode fields **except** scope using this builder. The scope
/// is injected exactly once by [`ScopedLunaris.ingest`]. This makes it
/// impossible to construct an Episode with an arbitrary scope by bypassing
/// the `ScopedLunaris` wrapper.
///
/// ```python
/// from lunaris import EpisodeBuilder, Scope
///
/// builder = (
///     EpisodeBuilder("agent:fs/report.md", "# Q3 Report\n...")
///     .t_ref("2026-01-01T00:00:00Z")
///     .metadata({"author": "helios"})
/// )
/// lsn = await engine.scoped(Scope("acme:agent-1")).ingest(builder)
/// ```
#[pyclass(name = "EpisodeBuilder", dict)]
#[derive(Clone)]
pub struct PyEpisodeBuilder {
    pub(crate) inner: ::lunaris::EpisodeBuilder,
}

#[pymethods]
impl PyEpisodeBuilder {
    /// Construct a builder with the required `source` and `content`.
    ///
    /// `source` is the namespace-qualified origin (e.g. `"helios:fs/report.md"`).
    /// `content` is the raw text that will be chunked + embedded.
    #[new]
    fn new(source: String, content: String) -> PyResult<Self> {
        Ok(Self { inner: ::lunaris::EpisodeBuilder::new(source, content) })
    }

    /// Set the reference timestamp (valid time anchor).
    ///
    /// Accepts an ISO-8601 string (e.g. `"2026-01-01T00:00:00Z"`). When not
    /// set the ingest pipeline uses the current wall time from the HlcClock.
    ///
    /// Returns a new `EpisodeBuilder` (builder pattern).
    fn t_ref(slf: PyRef<'_, Self>, t: &str) -> PyResult<PyEpisodeBuilder> {
        let dt: chrono::DateTime<chrono::Utc> = t.parse().map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "EpisodeBuilder.t_ref: invalid ISO-8601 datetime {t:?}: {e}"
            ))
        })?;
        Ok(PyEpisodeBuilder { inner: slf.inner.clone().t_ref(dt) })
    }

    /// Merge metadata key/value pairs into the builder.
    ///
    /// `m` must be a Python dict whose keys are strings. The values must be
    /// JSON-serialisable.
    ///
    /// Returns a new `EpisodeBuilder` (builder pattern).
    fn metadata(slf: PyRef<'_, Self>, m: &Bound<'_, PyDict>) -> PyResult<PyEpisodeBuilder> {
        let map: serde_json::Map<String, serde_json::Value> =
            pythonize::depythonize(m).map_err(|e| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "EpisodeBuilder.metadata: serialisation failed: {e}"
                ))
            })?;
        Ok(PyEpisodeBuilder { inner: slf.inner.clone().metadata(map) })
    }

    fn __repr__(&self) -> String {
        "EpisodeBuilder(...)".to_string()
    }
}

// ---------------------------------------------------------------------------
// PyScopedLunaris
// ---------------------------------------------------------------------------

/// Scope-bound view over a [`Lunaris`] handle.
///
/// Constructed via `engine.scoped(scope)`. All operations issued through this
/// wrapper carry the bound scope as their partitioning key.
///
/// ```python
/// from lunaris import Scope, EpisodeBuilder
///
/// scoped = engine.scoped(Scope("acme:agent-1"))
/// lsn = await scoped.ingest(EpisodeBuilder("src", "content"))
/// hits = await scoped.recall("brown fox")
/// builder = scoped.dsl()   # RetrievalBuilder pre-seeded with the scope
/// ```
///
/// ## Lifetime note
///
/// The Python-side wrapper owns `Arc<Lunaris>` (not a borrow) so it is safe
/// to pass around without lifetime constraints. On every method call the Rust
/// side re-derives `ScopedLunaris<'_>` via `inner.scoped(scope.clone())` —
/// this is a zero-allocation stack borrow under the Arc.
#[pyclass(name = "ScopedLunaris", dict)]
pub struct PyScopedLunaris {
    pub(crate) inner: Arc<::lunaris::Lunaris>,
    pub(crate) scope: ::lunaris::Scope,
}

#[pymethods]
impl PyScopedLunaris {
    /// Returns the scope this view is bound to as a `Scope` object.
    #[getter]
    fn scope(&self) -> PyScope {
        PyScope { inner: self.scope.clone() }
    }

    /// Ingest an episode payload under the bound scope.
    ///
    /// `builder` must be an `EpisodeBuilder`. The scope is stamped onto the
    /// episode by this method — callers cannot inject an arbitrary scope.
    ///
    /// Returns the string-formatted `Lsn` (e.g. `"1746000000000:1"`).
    fn ingest<'py>(
        &self,
        py: Python<'py>,
        builder: PyRef<'_, PyEpisodeBuilder>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let scope = self.scope.clone();
        let ep_builder = builder.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // Re-derive the borrow inside the future — `ScopedLunaris<'_>`
            // lifetime is local to this async block, safe without unsafe.
            let scoped = inner.scoped(scope);
            let lsn = scoped.ingest(ep_builder).await.map_err(py_err)?;
            Python::attach(|py| Ok(lsn.to_string().into_pyobject(py)?.into_any().unbind()))
        })
    }

    /// Recall hits under the bound scope using a text query string.
    ///
    /// Returns a list of hit dicts via `pythonize::pythonize`. The scope is
    /// threaded through the entire retrieval path — only hits from this scope's
    /// partition are returned.
    fn recall<'py>(&self, py: Python<'py>, query: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let scope = self.scope.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let scoped = inner.scoped(scope);
            let q = ::lunaris::Query::text(&query);
            let hits = scoped.recall(q).await.map_err(py_err)?;
            Python::attach(|py| Ok(pythonize::pythonize(py, &hits)?.unbind()))
        })
    }

    /// Return a `RetrievalBuilder` pre-seeded with this scope.
    ///
    /// Equivalent to `engine.recall().with_scope(scope)` but exposed here for
    /// DSL-style composition without holding an extra reference to the engine.
    fn dsl(&self) -> crate::generated::PyRetrievalBuilder {
        let builder = self.inner.scoped(self.scope.clone()).dsl();
        crate::generated::PyRetrievalBuilder { inner: Arc::new(builder) }
    }

    fn __repr__(&self) -> String {
        format!("ScopedLunaris(scope={:?})", self.scope.as_str())
    }
}

// ---------------------------------------------------------------------------
// Free function: lunaris_scoped — factory exposed in the pymodule
// ---------------------------------------------------------------------------

/// Construct a `ScopedLunaris` bound to `scope`.
///
/// Exposed as a free function so the `Lunaris` class can be patched with a
/// `scoped` method via the pure-Python `__init__.py` layer rather than
/// requiring a codegen change or `generated.rs` edit.
///
/// Python callers use `engine.scoped(scope)` — the `__init__.py` wires this:
///
/// ```python
/// # python/lunaris/__init__.py
/// Lunaris.scoped = lambda self, scope: lunaris_scoped(self, scope)
/// ```
#[pyfunction]
#[pyo3(signature = (handle, scope))]
pub(crate) fn lunaris_scoped(
    handle: PyRef<'_, PyLunaris>,
    scope: PyRef<'_, PyScope>,
) -> PyResult<PyScopedLunaris> {
    Ok(PyScopedLunaris { inner: handle.inner.clone(), scope: scope.inner.clone() })
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyScope>()?;
    m.add_class::<PyEpisodeBuilder>()?;
    m.add_class::<PyScopedLunaris>()?;
    m.add_function(wrap_pyfunction!(lunaris_scoped, m)?)?;
    Ok(())
}
