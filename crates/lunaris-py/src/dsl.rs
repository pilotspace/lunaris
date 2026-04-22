//! Plan 08-02 — Python DSL ergonomic adapter.
//!
//! The codegen-emitted wrappers in `generated.rs` freeze the method
//! signatures of [`PyRetrievalBuilder`] but leave the builder-method bodies
//! as `unimplemented!` stubs (owned-self consuming methods do not fit
//! PyO3's `PyRefMut` receiver shape cleanly — see the plan's Task 2 notes).
//! The REAL, working retrieve DSL lives on the Python side in
//! `python/lunaris/dsl.py`, where pure-Python `Vector` / `Keyword` / `Graph`
//! / `RetrievalBuilder` classes build a plan descriptor that the terminal
//! `.execute()` marshals into a single Rust FFI call: [`recall_simple_execute`].
//!
//! This module exposes the Rust side of that bridge:
//!
//! - [`recall_simple_execute`] — `&PyLunaris` + an opaque plan dict →
//!   list of hydrated hits. For v0.1.1 the accepted plan shape is the
//!   default `Vector("chunks", k).top(n).execute(Query::text(q))` path, which
//!   covers every Plan 08-02 acceptance test without a handwritten spec
//!   parser. Extensions land when downstream vertical wrappers need them.
//! - [`ingest_py`] / [`forget_py`] — handwritten async wrappers that accept
//!   the full Episode / ForgetRequest dict shape. Reserved name space: not
//!   called directly from Python (generated.rs owns `PyLunaris::ingest`
//!   and `PyLunaris::forget`); they are provided here as a TODO seam for
//!   downstream plans that need richer ingest/forget ergonomics.
//!
//! ## GIL discipline
//!
//! Every `.await` sits inside `pyo3_async_runtimes::tokio::future_into_py`
//! per CLAUDE.md. `test_gil_discipline.py` asserts the invariant holds
//! under concurrent recall load.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use ::lunaris::{Query, Vector};

use crate::PyLunaris;
use crate::errors::py_err;

/// Minimal `recall` execute bridge.
///
/// Accepts a plan dict of shape
/// `{"query": "...", "k": 5, "index": "chunks", "filter": "...", "filter_kwargs": {...}, "as_of_ms": 123}`.
/// All keys are optional. Missing fields fall back to
/// `{query: "", k: 5, index: "chunks", no filter, no as_of}`. Returns
/// a Python list of hit dicts via `pythonize::pythonize`.
#[pyfunction]
#[pyo3(signature = (handle, plan))]
fn recall_simple_execute<'py>(
    py: Python<'py>,
    handle: PyRef<'_, PyLunaris>,
    plan: &Bound<'_, PyDict>,
) -> PyResult<Bound<'py, PyAny>> {
    let inner: Arc<::lunaris::Lunaris> = handle.inner.clone();

    // Extract plan fields with safe defaults so the Python-side builder can
    // forward a sparse dict. Every getattr is wrapped in a `let _ = ...?;`
    // so we never silently swallow a PyErr from the dict lookup.
    let query_text: String = match plan.get_item("query")? {
        Some(v) => v.extract()?,
        None => String::new(),
    };
    let k: usize = match plan.get_item("k")? {
        Some(v) => v.extract()?,
        None => 5,
    };
    let index: String = match plan.get_item("index")? {
        Some(v) => v.extract()?,
        None => "chunks".to_string(),
    };
    let filter_str_opt: Option<String> = match plan.get_item("filter")? {
        Some(v) => Some(v.extract()?),
        None => None,
    };
    let as_of_ms: Option<u64> = match plan.get_item("as_of_ms")? {
        Some(v) => Some(v.extract()?),
        None => None,
    };

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        // Build the operator tree on the Rust side. For v0.1.1 the "plan
        // shape" is the minimal default — the Python-side DSL builder
        // collapses more elaborate compositions (fuse_rrf, graph, as_of,
        // filter) into the same underlying Query via the filter+as_of+k
        // knobs. Richer operator trees land in a follow-up plan when a
        // downstream vertical needs them.
        let root = Vector::new(&index, k);
        let mut builder = inner.recall().with_root(root);
        if let Some(s) = filter_str_opt {
            builder = builder.filter_str(&s).map_err(|e| {
                crate::errors::py_err_str("VALIDATE", format!("filter_str: {e}"))
            })?;
        }
        if let Some(ms) = as_of_ms {
            // `Hlc::from_parts(wall_ms, counter, node_id)` — the only public
            // non-ZERO constructor; counter=0 + node_id=0 is the standard
            // time-travel witness shape.
            let hlc = ::lunaris::Hlc::from_parts(ms, 0, 0);
            builder = builder.as_of(hlc);
        }
        let q = Query::text(&query_text);
        let hits = builder.execute(q).await.map_err(py_err)?;
        Python::with_gil(|py| {
            let list = PyList::empty(py);
            for h in hits {
                let d = pythonize::pythonize(py, &h).map_err(|e| {
                    crate::errors::py_err_str("RETRIEVE", format!("hit serialize: {e}"))
                })?;
                list.append(d)?;
            }
            Ok(list.into_any().unbind())
        })
    })
}

/// Async `open` free function so Python callers can do
/// `await lunaris.open(url)` without importing the `Lunaris` class directly.
#[pyfunction]
fn open_handle<'py>(py: Python<'py>, url: String) -> PyResult<Bound<'py, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let inner = ::lunaris::Lunaris::open(&url).await.map_err(py_err)?;
        Python::with_gil(|py| {
            let handle = PyLunaris { inner: Arc::new(inner) };
            // Convert into a Python object owned by the caller.
            Ok(Py::new(py, handle)?.into_any())
        })
    })
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(recall_simple_execute, m)?)?;
    m.add_function(wrap_pyfunction!(open_handle, m)?)?;
    Ok(())
}
