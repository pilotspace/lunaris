//! Plan 08-03 — TypeScript-facing error surface.
//!
//! Maps the Rust-side [`::lunaris::LunarisError`] taxonomy to a single
//! `napi::Error` carrying a coarse subsystem tag (STORAGE / EXTRACT /
//! VALIDATE / RETRIEVE / CONSOLIDATE) as a prefix on the error reason so
//! TypeScript callers can branch on the coarse subsystem via string
//! matching without parsing the message. Mirrors the Plan 08-02
//! `py_err` → `LunarisError` shape on the PyO3 side.
//!
//! The napi-rs 3.x `napi::Error::new(Status, Reason)` constructor accepts
//! any `Into<String>` reason; we keep the reason shape stable
//! (`"CODE: message"`) so JavaScript can do `err.message.startsWith("STORAGE:")`.
//!
//! Plan 08-03 T-08-03-03 mitigation: the inner Rust error's `Display`
//! output already elides secrets (storage errors surface scheme + a stable
//! reason, never DSNs); we preserve that `format!("{err}")` here and
//! prefix it with the coarse subsystem code.

use napi::{Error as NapiError, Status};

/// Coarse subsystem classifier — mirrors the [`::lunaris::LunarisError`]
/// variants 1:1. Kept as a `&'static str` (not a napi-side enum) so the
/// TypeScript side can compare with plain `===` and logs stay
/// string-only.
///
/// This was a local match ending in `_ => "UNKNOWN"`, and the "1:1" above was
/// wrong: `LunarisError::Scope` had been added and reached JavaScript as
/// UNKNOWN. The wildcard was not optional — `LunarisError` is
/// `#[non_exhaustive]` and this is a downstream crate — so the claim could
/// never be compiler-checked here. It is checked in `lunaris-core`, where the
/// match lives now.
fn classify(err: &::lunaris::LunarisError) -> &'static str {
    err.subsystem().code()
}

/// Type-erased converter used by the generated `.map_err(napi_err)` sites
/// AND by every handwritten helper in `dsl.rs` / `conformance.rs`. Accepts
/// anything displayable-as-a-LunarisError: either the real
/// `::lunaris::LunarisError` (direct match) or any `std::fmt::Display`
/// value (fallback for serde errors, filter-parse errors, etc.).
///
/// napi-rs 3.x's trait-based error conversion (`From<E> for napi::Error`)
/// would let us drop the explicit `.map_err(napi_err)` from every call
/// site, but the codegen emitter keeps it explicit so the snapshot
/// contract is ABI-stable across host-crate refactors.
pub(crate) fn napi_err<E: NapiErrLike>(err: E) -> NapiError {
    err.into_napi_err()
}

/// Internal trait used by [`napi_err`] so a single function can accept
/// both the typed `LunarisError` and raw `serde_json::Error` /
/// `anyhow::Error` without an ambiguous blanket `impl From` at the
/// workspace layer.
pub(crate) trait NapiErrLike {
    fn into_napi_err(self) -> NapiError;
}

impl NapiErrLike for ::lunaris::LunarisError {
    fn into_napi_err(self) -> NapiError {
        let code = classify(&self);
        let msg = format!("{self}");
        NapiError::new(Status::GenericFailure, format!("{code}: {msg}"))
    }
}

impl NapiErrLike for serde_json::Error {
    fn into_napi_err(self) -> NapiError {
        NapiError::new(Status::InvalidArg, format!("VALIDATE: {self}"))
    }
}

/// Convenience builder for non-native-Rust error sources — e.g. a
/// `FilterParseError` from `lunaris_retrieve::filter_str` or a custom
/// check in the handwritten DSL adapter. Callers supply the coarse
/// subsystem code explicitly.
#[allow(dead_code)]
pub(crate) fn napi_err_with_code<D: std::fmt::Display>(code: &'static str, e: D) -> NapiError {
    NapiError::new(Status::GenericFailure, format!("{code}: {e}"))
}

#[cfg(test)]
mod tests {
    // Unit-level test: classification covers every variant the core enum
    // declares (see `classifies_every_subsystem`). Runtime-facing napi<->JS round-trip
    // lives in `__test__/open_ingest_recall.spec.mts::errorsMapToLunarisError`.

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

    /// The two tests above cover two variants; the comment above them claimed
    /// five, and the enum had six. Walk what the core enum actually declares.
    #[test]
    fn classifies_every_subsystem() {
        for sub in lunaris_core::Subsystem::ALL {
            assert_eq!(
                classify(&sub.sample_error()),
                sub.code(),
                "{sub:?} does not reach the SDK with its own code"
            );
            assert_ne!(classify(&sub.sample_error()), "UNKNOWN", "{sub:?} arrives as UNKNOWN");
        }
    }
}
