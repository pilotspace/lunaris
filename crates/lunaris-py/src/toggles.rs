//! Plan 08-02 BIND-PY-04 — three-surface pipeline toggles (code + env + config).
//!
//! The codegen-emitted [`crate::PyGraphPipelineHandle`] +
//! [`crate::PyConsolidatorPipelineHandle`] classes in `generated.rs` expose
//! the single `toggle(on: bool)` entry (the canonical single-item binding
//! surface per Plan 08-01). This module layers ergonomic `enable()` /
//! `disable()` / `is_enabled()` accessors that Python callers reach via
//! `handle.graph_pipeline.enable()` — the code-surface path from
//! `<key_constraints>`.
//!
//! ## Three surfaces
//!
//! 1. **Code**: `handle.graph_pipeline.enable()` — calls
//!    [`::lunaris::GraphPipelineHandle::enable`] directly; no env or config
//!    involved. Immediate.
//! 2. **Env**: `LUNARIS_GRAPH_ENABLED=1` at `lunaris.open(url)` time — the
//!    Rust-side `Lunaris::open` already reads the env via
//!    [`::lunaris::GraphPipelineHandle::initial_state_from_env`], so the
//!    Python wrapper inherits this for free.
//! 3. **Config dict**: `await lunaris.open(url, config={"graph_pipeline":
//!    {"enabled": True}})` — handled by the Python-side `dsl.py::open`
//!    wrapper, which walks the dict AFTER construction and calls
//!    `handle.graph_pipeline.enable()` when `enabled=True`.
//!
//! Surface-precedence resolution (code > env > config) is enforced by the
//! Python wrapper: the wrapper always calls `enable()` when the dict says so,
//! overriding env-initial state. Code surfaces run AFTER `lunaris.open()`
//! returns, so they are the final state.
//!
//! ## Exposed free helpers
//!
//! [`from_env`] / [`from_config`] helpers read the three-surface inputs and
//! return the final (bool, bool) tuple that the Python wrapper applies to
//! `handle.graph_pipeline` / `handle.consolidator_pipeline`. They exist so
//! tests can assert the resolution order without constructing a real handle.

use pyo3::prelude::*;
use pyo3::types::PyDict;

use std::sync::Arc;

use crate::{PyConsolidatorPipelineHandle, PyGraphPipelineHandle, PyLunaris};

// =====================================================================
// Ergonomic methods on the codegen-frozen PyFoo classes.
//
// We CANNOT add a second `#[pymethods]` impl block without enabling the
// `multiple-pymethods` PyO3 feature (which pulls `inventory` + a small
// runtime registration cost). Instead, we expose the ergonomic surface
// via free `#[pyfunction]` helpers that take the PyFoo by `&self` and
// call into the inner Rust handle. The Python-side wrapper module in
// `python/lunaris/toggles.py` attaches them as methods on the class.
// =====================================================================

/// Return a `PyGraphPipelineHandle` wrapping the inner Rust handle.
/// Surfaced to Python as `handle.graph_pipeline` via the
/// `python/lunaris/__init__.py` wrapper's property on `Lunaris`.
#[pyfunction]
fn get_graph_pipeline(handle: PyRef<'_, PyLunaris>) -> PyResult<PyGraphPipelineHandle> {
    let inner: Arc<::lunaris::GraphPipelineHandle> = handle.inner.graph_pipeline();
    Ok(PyGraphPipelineHandle { inner })
}

/// Return a `PyConsolidatorPipelineHandle` wrapping the inner Rust handle.
#[pyfunction]
fn get_consolidator_pipeline(
    handle: PyRef<'_, PyLunaris>,
) -> PyResult<PyConsolidatorPipelineHandle> {
    let inner: Arc<::lunaris::ConsolidatorPipelineHandle> = handle.inner.consolidator_pipeline();
    Ok(PyConsolidatorPipelineHandle { inner })
}

// Pipeline `enable()` internally calls `tokio::spawn(...)` (see
// `crates/lunaris/src/consolidator_pipeline.rs:121` + `graph_pipeline.rs`
// counterpart), which panics with "no reactor running" when invoked from
// the Python main thread without an active tokio runtime. Entering the
// pyo3-async-runtimes shared runtime handle for the duration of the call
// satisfies `Handle::current()` so `tokio::spawn` works.
//
// `is_enabled()` is a pure atomic-load — safe to call without the guard.
fn with_tokio<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let rt = pyo3_async_runtimes::tokio::get_runtime();
    let _g = rt.handle().enter();
    f()
}

#[pyfunction]
fn graph_pipeline_enable(handle: PyRef<'_, PyGraphPipelineHandle>) -> PyResult<()> {
    with_tokio(|| handle.inner.enable());
    Ok(())
}

#[pyfunction]
fn graph_pipeline_disable(handle: PyRef<'_, PyGraphPipelineHandle>) -> PyResult<()> {
    with_tokio(|| handle.inner.disable());
    Ok(())
}

#[pyfunction]
fn graph_pipeline_is_enabled(handle: PyRef<'_, PyGraphPipelineHandle>) -> PyResult<bool> {
    Ok(handle.inner.is_enabled())
}

#[pyfunction]
fn consolidator_pipeline_enable(handle: PyRef<'_, PyConsolidatorPipelineHandle>) -> PyResult<()> {
    with_tokio(|| handle.inner.enable());
    Ok(())
}

#[pyfunction]
fn consolidator_pipeline_disable(handle: PyRef<'_, PyConsolidatorPipelineHandle>) -> PyResult<()> {
    with_tokio(|| handle.inner.disable());
    Ok(())
}

#[pyfunction]
fn consolidator_pipeline_is_enabled(
    handle: PyRef<'_, PyConsolidatorPipelineHandle>,
) -> PyResult<bool> {
    Ok(handle.inner.is_enabled())
}

/// Read the Rust-side env initial-state reader. Exposed so tests can assert
/// that the env surface is wired through to the underlying handle reader
/// without having to construct a real `Lunaris` handle.
#[pyfunction]
fn from_env() -> PyResult<(bool, bool)> {
    let g = ::lunaris::GraphPipelineHandle::initial_state_from_env();
    let c = ::lunaris::ConsolidatorPipelineHandle::initial_state_from_env();
    Ok((g, c))
}

/// Pure decision helper mirroring the Rust-side
/// [`::lunaris::GraphPipelineHandle::initial_state_from_value`] + equivalent
/// consolidator reader. Takes a config dict of shape
/// `{"graph_pipeline": {"enabled": bool}, "consolidator_pipeline": {"enabled": bool}}`
/// and returns `(graph_enabled, consolidator_enabled)`. Missing keys default
/// to `false` (default OFF per blueprint §5.1/§5.2).
#[pyfunction]
fn from_config(config: &Bound<'_, PyDict>) -> PyResult<(bool, bool)> {
    let g = read_nested_enabled(config, "graph_pipeline")?;
    let c = read_nested_enabled(config, "consolidator_pipeline")?;
    Ok((g, c))
}

fn read_nested_enabled(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<bool> {
    let nested = match dict.get_item(key)? {
        Some(v) => v,
        None => return Ok(false),
    };
    let nested_dict: &Bound<'_, PyDict> = match nested.cast::<PyDict>() {
        Ok(d) => d,
        Err(_) => return Ok(false),
    };
    let enabled_any = match nested_dict.get_item("enabled")? {
        Some(v) => v,
        None => return Ok(false),
    };
    enabled_any.extract()
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(get_graph_pipeline, m)?)?;
    m.add_function(wrap_pyfunction!(get_consolidator_pipeline, m)?)?;
    m.add_function(wrap_pyfunction!(graph_pipeline_enable, m)?)?;
    m.add_function(wrap_pyfunction!(graph_pipeline_disable, m)?)?;
    m.add_function(wrap_pyfunction!(graph_pipeline_is_enabled, m)?)?;
    m.add_function(wrap_pyfunction!(consolidator_pipeline_enable, m)?)?;
    m.add_function(wrap_pyfunction!(consolidator_pipeline_disable, m)?)?;
    m.add_function(wrap_pyfunction!(consolidator_pipeline_is_enabled, m)?)?;
    m.add_function(wrap_pyfunction!(from_env, m)?)?;
    m.add_function(wrap_pyfunction!(from_config, m)?)?;
    Ok(())
}
