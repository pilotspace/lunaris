//! Plan 08-02 — Python-facing error surface.
//!
//! Maps the Rust-side [`::lunaris::LunarisError`] taxonomy to a single
//! `lunaris.LunarisError` Python exception class (subclass of `RuntimeError`)
//! whose repr carries `code` + `message` so Python callers can branch on the
//! coarse subsystem without parsing the message.
//!
//! Plan 08-02 T-08-02-03 mitigation: the inner Rust error's `Display` output
//! already elides secrets (storage errors surface scheme + a stable reason,
//! never DSNs); we preserve that `format!("{err}")` here and prefix it with
//! the coarse subsystem code so Python-side logs stay grep-friendly.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

// Top-level Python exception. Exposed to Python as `lunaris.LunarisError`.
// Subclass of `RuntimeError` so `try/except RuntimeError` callers keep
// working; PyO3 populates the exception args from our tuple below so
// `e.args == (code, message)` holds on the Python side.
pyo3::create_exception!(
    lunaris,
    LunarisError,
    PyRuntimeError,
    "Lunaris top-level error. `args == (code: str, message: str)` — the `code` \
     is a coarse subsystem tag (STORAGE / EXTRACT / VALIDATE / RETRIEVE / \
     CONSOLIDATE), `message` is the `Display`-formatted inner error."
);

/// Coarse subsystem classifier — mirrors the [`::lunaris::LunarisError`]
/// variants 1:1. Kept as a `&'static str` (not a Python enum) so the Python
/// side can compare with plain `==` and logs stay string-only.
fn classify(err: &::lunaris::LunarisError) -> &'static str {
    match err {
        ::lunaris::LunarisError::Storage(_) => "STORAGE",
        ::lunaris::LunarisError::Extract(_) => "EXTRACT",
        ::lunaris::LunarisError::Validate(_) => "VALIDATE",
        ::lunaris::LunarisError::Retrieve(_) => "RETRIEVE",
        ::lunaris::LunarisError::Consolidate(_) => "CONSOLIDATE",
    }
}

/// Convert a [`::lunaris::LunarisError`] into a [`PyErr`] of type
/// `lunaris.LunarisError`. Called by every codegen-emitted `.map_err(py_err)`
/// site plus the handwritten wrappers in `dsl.rs` / `conformance.rs`.
pub(crate) fn py_err(err: ::lunaris::LunarisError) -> PyErr {
    let code = classify(&err);
    let msg = format!("{err}");
    // Two-tuple args so Python can unpack `code, message = exc.args`.
    LunarisError::new_err((code, msg))
}

/// Convenience overload for anything implementing `Display` — used by the
/// `dsl.rs` helpers that synthesize a LunarisError from a non-native-Rust
/// source (e.g. a parse failure on the `filter(**kwargs)` shim).
#[allow(dead_code)]
pub(crate) fn py_err_str<D: std::fmt::Display>(code: &'static str, e: D) -> PyErr {
    LunarisError::new_err((code, format!("{e}")))
}

#[cfg(test)]
mod tests {
    // Unit-level test: classification covers all 5 variants from
    // `lunaris-core/src/error.rs:5-17` without hitting PyO3 runtime. The
    // Python-side round-trip (`.code` / `.message` readable from Python) is
    // covered by `tests/test_open_ingest_recall.py::test_errors_map_to_lunaris_error`.

    use super::classify;
    use ::lunaris::LunarisError;
    use lunaris_core::{StorageError, ValidateError};

    #[test]
    fn classifies_storage_variant() {
        let err = LunarisError::Storage(StorageError::UnsupportedScheme("foo".into()));
        assert_eq!(classify(&err), "STORAGE");
    }

    #[test]
    fn classifies_validate_variant() {
        let err = LunarisError::Validate(ValidateError::Temporal);
        assert_eq!(classify(&err), "VALIDATE");
    }
}
