//! NOT codegen-managed — conformance-only helpers (Plan 08-04).
//!
//! Exposed only when the `bindings-it` feature is enabled. Production PyPI
//! wheels NEVER ship this module. The `lunaris-codegen` parity-check walker
//! explicitly excludes any file named `conformance.rs` via the explicit
//! three-path enumeration in `codegen_managed_paths` (Plan 08-01
//! T-08-01-05 carveout).
//!
//! ## Surface (revision iteration 2 — trimmed)
//!
//! - `conformance_fixture_episodes(seed, count) -> list[dict]` — returns
//!   `FixtureCorpus::episodes()` serialised to Python dicts. `seed` and
//!   `count` arguments are asserted against the canonical Rust constants
//!   so silent cross-language drift surfaces as a hard assertion at
//!   the FFI boundary.
//! - `scan_kv_prefix(handle, prefix) -> list[tuple[bytes, bytes]]` —
//!   thin wrapper over `StoragePort::scan_range(prefix, None) →
//!   futures::TryStreamExt::try_collect`. Lets Plan 08-04 drivers dump
//!   every KV row under a prefix for the Moon-vs-Postgres parity check.
//!
//! Earlier revisions also exposed `with_zero_embedder` and
//! `with_clock_seed`. BOTH REMOVED in revision iteration 2 — Plan 08-04's
//! per-driver backend parity test does NOT require embedder or clock
//! determinism (a single driver process writes identical bytes to both
//! backends via `StoragePort::atomic_write`). Keeping the surface minimal
//! also keeps the `lunaris-core` entropy sources (`primitives::Ulid::new()`,
//! `hlc.rs`) untouched.
//!
//! ## GIL discipline (CLAUDE.md)
//!
//! The async `scan_kv_prefix` helper sits inside `future_into_py` per the
//! CLAUDE.md invariant enforced at codegen time and reiterated here in
//! handwritten code. No `.await` escapes the closure.

#![cfg(feature = "bindings-it")]

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyList, PyTuple};

use ::lunaris_conformance::fixtures::FixtureCorpus;

use crate::PyLunaris;
use crate::errors::py_err;

/// Returns a list of dicts mirroring `FixtureCorpus::episodes()`.
///
/// `seed` and `count` MUST match `lunaris_conformance::fixtures::SEED` /
/// `EPISODE_COUNT` — we accept them as args so drivers can assert the
/// constants at the boundary, but the underlying `FixtureCorpus::new()`
/// always uses the canonical Rust constants. Drift triggers a hard panic
/// with the canonical Rust value in the message so operators know which
/// side of the FFI needs updating.
#[pyfunction]
fn conformance_fixture_episodes(
    py: Python<'_>,
    seed: u64,
    count: usize,
) -> PyResult<Py<PyList>> {
    assert_eq!(
        seed,
        ::lunaris_conformance::fixtures::SEED,
        "conformance seed drift — Rust constant is 0x{:x}",
        ::lunaris_conformance::fixtures::SEED
    );
    assert_eq!(
        count,
        ::lunaris_conformance::fixtures::EPISODE_COUNT,
        "conformance episode-count drift — Rust constant is {}",
        ::lunaris_conformance::fixtures::EPISODE_COUNT
    );
    let corpus = FixtureCorpus::new();
    let list = PyList::empty(py);
    for ep in corpus.episodes() {
        let dict = pythonize::pythonize(py, ep).map_err(|e| {
            PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("pythonize: {e}"))
        })?;
        list.append(dict)?;
    }
    Ok(list.into())
}

/// Thin wrapper over `StoragePort::scan_range(prefix, None)` →
/// `try_collect::<Vec<_>>()`. Returns a Python list of `(bytes, bytes)`
/// tuples — one per KV row under the prefix.
///
/// Plan 08-04's driver dumps every row under the fixture-corpus
/// write prefixes from Moon and Postgres separately, then asserts the
/// two dumps are structurally-equal against a committed golden
/// reference. Exposed as a free `#[pyfunction]` rather than a method
/// on `PyLunaris` because a second `#[pymethods] impl PyLunaris` block
/// would require the `multiple-pymethods` PyO3 feature — cheaper to
/// route through the handle-by-ref shape below.
#[pyfunction]
fn scan_kv_prefix<'py>(
    py: Python<'py>,
    handle: PyRef<'_, PyLunaris>,
    prefix: &[u8],
) -> PyResult<Bound<'py, PyAny>> {
    let storage = handle.inner.storage();
    let prefix: Vec<u8> = prefix.to_vec();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        use futures::TryStreamExt;
        let stream = storage
            .scan_range(&prefix, None)
            .await
            .map_err(|e| py_err(::lunaris::LunarisError::Storage(e)))?;
        let rows: Vec<(bytes::Bytes, bytes::Bytes)> = stream
            .try_collect()
            .await
            .map_err(|e| py_err(::lunaris::LunarisError::Storage(e)))?;
        Python::with_gil(|py| {
            let out = PyList::empty(py);
            for (k, v) in rows {
                let k_bytes = PyBytes::new(py, &k);
                let v_bytes = PyBytes::new(py, &v);
                let tup = PyTuple::new(py, [k_bytes.into_any(), v_bytes.into_any()])?;
                out.append(tup)?;
            }
            Ok(out.unbind().into_any())
        })
    })
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(conformance_fixture_episodes, m)?)?;
    m.add_function(wrap_pyfunction!(scan_kv_prefix, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // Unit-level check that the handwritten Rust side compiles and that
    // FixtureCorpus is reachable under the feature gate. Full Python
    // integration (via conformance_fixture_episodes) is exercised by
    // Plan 08-04's backend-parity driver.

    #[test]
    fn test_conformance_helpers_exist() {
        let corpus = ::lunaris_conformance::fixtures::FixtureCorpus::new();
        assert_eq!(
            corpus.episodes().len(),
            ::lunaris_conformance::fixtures::EPISODE_COUNT
        );
    }
}
