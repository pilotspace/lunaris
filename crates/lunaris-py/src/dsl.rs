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
//!   list of hydrated hits. The plan carries a `"root"` operator tree that
//!   `lunaris_retrieve::plan::retriever_from_json` turns into the retriever,
//!   so a composed plan (`.and_()`, `.fuse_rrf()`, `.top()`, a graph leg)
//!   runs as the caller wrote it (F14). The pre-F14 flat `{index, k}` shape
//!   is still accepted and is normalized into the same single-`vector` tree,
//!   so there is exactly ONE place that decides what a plan means.
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

use ::lunaris::Query;

use crate::PyLunaris;
use crate::errors::py_err;

/// Minimal `recall` execute bridge.
///
/// Accepts a plan dict of shape
/// `{"root": {...}, "query": "...", "filter": "...", "as_of_ms": 123,
/// "scope": "acme-agent-1"}`, where
/// `root` is the operator tree documented on
/// [`lunaris_retrieve::plan`]. All keys are optional.
///
/// When `root` is absent the flat pre-F14 fields (`index`, `k`) are read
/// instead and normalized into the equivalent one-leg `vector` tree — the
/// legacy shape is a spelling of a plan, not a second execution path, so a
/// change to how plans are built cannot apply to one caller and miss the
/// other. Missing fields fall back to `{query: "", k: 5, index: "chunks"}`.
///
/// Returns a Python list of hit dicts via `pythonize::pythonize`.
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
    // The operator tree. Absent = the pre-F14 flat shape, which is rewritten
    // into the one-leg tree that means the same thing rather than executed by
    // a parallel code path.
    let root_json: serde_json::Value = match plan.get_item("root")? {
        Some(v) => pythonize::depythonize(&v).map_err(|e| {
            crate::errors::py_err_str("VALIDATE", format!("plan root is not JSON-shaped: {e}"))
        })?,
        None => {
            let k: usize = match plan.get_item("k")? {
                Some(v) => v.extract()?,
                None => 5,
            };
            let index: String = match plan.get_item("index")? {
                Some(v) => v.extract()?,
                None => "chunks".to_string(),
            };
            serde_json::json!({"op": "vector", "index": index, "k": k})
        }
    };
    let filter_str_opt: Option<String> = match plan.get_item("filter")? {
        Some(v) => Some(v.extract()?),
        None => None,
    };
    let as_of_ms: Option<u64> = match plan.get_item("as_of_ms")? {
        Some(v) => Some(v.extract()?),
        None => None,
    };
    // W4.12 — the partition key. Envelope-level for the same reason `filter`
    // is: one scope narrows every leg of the plan. Absent means the caller
    // composed from the unscoped `handle.recall()`, and the engine falls back
    // to `Scope::dev()` with its own warning; a `scope` here comes from
    // `handle.scoped(s).dsl()` and MUST reach `with_scope` or the DSL silently
    // reads another tenant's partition.
    let scope_str: Option<String> = match plan.get_item("scope")? {
        Some(v) => Some(v.extract()?),
        None => None,
    };

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        // Build the operator tree on the Rust side from the plan the SDK
        // marshalled. `filter` / `as_of` / the query text stay envelope-level
        // because they are builder state in Rust too, not retrievers.
        let root = ::lunaris::retriever_from_json(&root_json)
            .map_err(|e| crate::errors::py_err_str("VALIDATE", format!("plan: {e}")))?;
        let mut builder = inner.recall();
        if let Some(s) = scope_str {
            let scope = ::lunaris::Scope::new(&s)
                .map_err(|e| crate::errors::py_err_str("VALIDATE", format!("scope: {e}")))?;
            builder = builder.with_scope(scope);
        }
        let mut builder = builder.with_root_boxed(root);
        if let Some(s) = filter_str_opt {
            builder = builder
                .filter_str(&s)
                .map_err(|e| crate::errors::py_err_str("VALIDATE", format!("filter_str: {e}")))?;
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
        Python::attach(|py| {
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
        Python::attach(|py| {
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
