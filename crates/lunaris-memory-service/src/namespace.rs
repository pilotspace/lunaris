//! Scratchpad namespace validation + resolution (transport-neutral).
//!
//! Moved verbatim (semantics-preserving) from `lunaris-mcp`'s `tools::staging`
//! so the scratchpad handlers can live in this shared crate and both client
//! surfaces (mcp proxy + contextd socket) validate identically.
//!
//! Namespace shapes the `scope_prefix` passed to `WorkingMemory::new`. It is
//! NOT a security boundary (scope is — bound at server startup / carried on the
//! trusted local socket), but it MUST NOT contain `:` to prevent source-key
//! byte-aliasing across namespaces — the same rationale as `Scope::new`'s
//! v0.2.1 alphabet tightening.

use crate::ServiceError;

/// Validate a caller-supplied namespace string.
///
/// Allowed: `[A-Za-z0-9_\-./]`, length 1..=128. `:` is rejected.
pub fn validate(ns: &str) -> Result<(), ServiceError> {
    if ns.is_empty() || ns.len() > 128 {
        return Err(ServiceError::InvalidInput(
            "namespace must be 1..=128 chars of [A-Za-z0-9_\\-./]; ':' is not allowed to prevent source-key aliasing".into(),
        ));
    }
    for c in ns.chars() {
        if !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '.' | '/') {
            return Err(ServiceError::InvalidInput(
                "namespace must be 1..=128 chars of [A-Za-z0-9_\\-./]; ':' is not allowed to prevent source-key aliasing".into(),
            ));
        }
    }
    Ok(())
}

/// Resolve the namespace from an optional value, validate it, and return it.
///
/// Returns `"scratchpad/"` when `None` (the legacy default). The mcp caller
/// pre-resolves the session-aware namespace into `Some(..)` before dispatch, so
/// `None` here only occurs for a direct/contextd caller that skipped session
/// resolution (or a test) — both correctly fall back to the legacy default.
pub fn resolve(ns: Option<String>) -> Result<String, ServiceError> {
    match ns {
        None => Ok("scratchpad/".to_owned()),
        Some(s) => {
            validate(&s)?;
            Ok(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_namespace_scratchpad_slash() {
        assert!(validate("scratchpad/").is_ok());
    }

    #[test]
    fn valid_namespace_single_char() {
        assert!(validate("a").is_ok());
    }

    #[test]
    fn valid_namespace_path_with_underscores_and_dots() {
        assert!(validate("path/with-underscores_and.dots/").is_ok());
    }

    #[test]
    fn valid_namespace_128_chars() {
        let s = "a".repeat(128);
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn invalid_namespace_empty() {
        assert!(validate("").is_err());
    }

    #[test]
    fn invalid_namespace_129_chars() {
        let s = "a".repeat(129);
        let msg = validate(&s).unwrap_err().to_string();
        assert!(msg.contains("128") || msg.contains("1..=128"), "expected length mention; got: {msg}");
    }

    #[test]
    fn invalid_namespace_colon() {
        let msg = validate("a:b").unwrap_err().to_string();
        assert!(msg.contains(':') || msg.contains("aliasing"), "expected ':' mention; got: {msg}");
    }

    #[test]
    fn invalid_namespace_space() {
        assert!(validate("has space").is_err());
    }

    #[test]
    fn invalid_namespace_at_sign() {
        assert!(validate("has@at").is_err());
    }

    #[test]
    fn resolve_none_is_legacy_default() {
        assert_eq!(resolve(None).unwrap(), "scratchpad/");
    }

    #[test]
    fn resolve_some_valid_passes_through() {
        assert_eq!(resolve(Some("notes/".into())).unwrap(), "notes/");
    }

    #[test]
    fn resolve_some_colon_rejected() {
        assert!(matches!(resolve(Some("a:b".into())), Err(ServiceError::InvalidInput(_))));
    }
}
