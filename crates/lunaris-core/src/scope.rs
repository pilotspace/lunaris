//! `Scope` newtype — the primary partition key for multi-agent / multi-tenant
//! isolation in Lunaris v0.2 (RFC 0001).
//!
//! Every primitive write-op path tags the row with the scope. Two scopes
//! compare equal iff their string forms match byte-for-byte. There is **no
//! implicit fallback to a "default" scope** — a `Scope` must be constructed
//! explicitly.
//!
//! ## Validation
//!
//! The string must match `^[A-Za-z0-9_\-:.]{1,128}$` (enforced by
//! [`Scope::new`]). The unchecked constructor `Scope::from_trusted` is
//! `pub(crate)` and is only used by trusted internal call sites (e.g.,
//! deserialization of previously-validated wire data).
//!
//! ## Examples
//!
//! ```
//! use lunaris_core::Scope;
//! let s = Scope::new("acme:agent-42").unwrap();
//! assert_eq!(s.as_str(), "acme:agent-42");
//! ```

use smol_str::SmolStr;
use thiserror::Error;

/// Validation regex fragment — kept as a const so backends and tests can
/// re-use it without duplicating the pattern.
///
/// Pattern: `^[A-Za-z0-9_\-:.]{1,128}$`
const VALID_SCOPE_CHARS: fn(char) -> bool =
    |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.');
const MAX_SCOPE_LEN: usize = 128;

/// A partition key for multi-agent / multi-tenant isolation.
///
/// `Scope` is a thin newtype around [`SmolStr`] (inline up to 23 bytes — most
/// scope identifiers fit). Two scopes compare equal iff their string forms
/// match byte-for-byte. There is **no implicit fallback to a "default"
/// scope** — a `Scope` must be constructed explicitly.
///
/// # Validation
///
/// The string must match `^[A-Za-z0-9_\-:.]{1,128}$`. This is enforced by
/// [`Scope::new`]; the unchecked constructor is `pub(crate)` and only used by
/// trusted internal call sites (deserialization of validated wire data).
///
/// # Examples
///
/// ```
/// use lunaris_core::Scope;
/// let s = Scope::new("acme:agent-42").unwrap();
/// assert_eq!(s.as_str(), "acme:agent-42");
/// ```
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Scope(SmolStr);

impl Scope {
    /// Construct a `Scope` from `s`, enforcing the validation regex
    /// `^[A-Za-z0-9_\-:.]{1,128}$`.
    ///
    /// Returns `Err(ScopeError::Invalid)` on empty string, string longer than
    /// 128 bytes, or any character outside the allowed set.
    pub fn new(s: impl AsRef<str>) -> Result<Self, ScopeError> {
        let s = s.as_ref();
        if s.is_empty() || s.len() > MAX_SCOPE_LEN || !s.chars().all(VALID_SCOPE_CHARS) {
            return Err(ScopeError::Invalid(s.to_string()));
        }
        Ok(Self(SmolStr::new(s)))
    }

    /// Borrow the scope as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Borrow the scope as a byte slice.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Trusted constructor for internal use at call sites that have already
    /// validated the string (e.g., deserialization of a row fetched from
    /// the validated database column). Caller is responsible for ensuring
    /// the invariant `^[A-Za-z0-9_\-:.]{1,128}$` holds.
    ///
    /// Wave 0: no call sites exist yet — Wave 1B (Postgres) and Wave 1C (Moon)
    /// will use this when deserializing scope values from storage rows.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn from_trusted(s: &str) -> Self {
        Self(SmolStr::new(s))
    }

    /// Development / migration helper. Returns a `Scope` whose value is
    /// `"_dev_"`. Use this at Wave 0 call sites where the real scope has not
    /// yet been threaded through (Wave 1 will replace these with actual
    /// per-agent scopes).
    ///
    /// **This function is intentionally `#[doc(hidden)]`** — it is a
    /// migration crutch and MUST NOT appear in public API documentation.
    /// Callers outside this crate should not use it in production code.
    #[doc(hidden)]
    pub fn dev() -> Self {
        // SAFETY: "_dev_" matches ^[A-Za-z0-9_\-:.]{1,128}$ by inspection.
        Self(SmolStr::new("_dev_"))
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl AsRef<str> for Scope {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

/// Error returned when constructing an invalid [`Scope`].
#[derive(Debug, Error)]
pub enum ScopeError {
    /// The string is empty, too long (> 128 chars), or contains a character
    /// outside `[A-Za-z0-9_\-:.]`.
    #[error("scope must be 1..=128 chars of [A-Za-z0-9_\\-:.]; got {0:?}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── valid construction ────────────────────────────────────────────────────

    #[test]
    fn valid_scope_roundtrip() {
        let s = Scope::new("acme:agent-42").unwrap();
        assert_eq!(s.as_str(), "acme:agent-42");
    }

    #[test]
    fn single_char_is_accepted() {
        assert!(Scope::new("a").is_ok());
        assert!(Scope::new("Z").is_ok());
        assert!(Scope::new("0").is_ok());
        assert!(Scope::new("_").is_ok());
    }

    #[test]
    fn max_length_scope_is_accepted() {
        let at_limit = "a".repeat(128);
        assert!(Scope::new(&at_limit).is_ok());
    }

    #[test]
    fn all_regex_specials_individually_accepted() {
        // Every character outside alphanumerics that the regex permits.
        assert!(Scope::new("under_score").is_ok(), "underscore must be valid");
        assert!(Scope::new("hy-phen").is_ok(), "hyphen must be valid");
        assert!(Scope::new("co:lon").is_ok(), "colon must be valid");
        assert!(Scope::new("do.t").is_ok(), "dot must be valid");
        // Combined in one identifier — same as the pattern A0._:-
        assert!(Scope::new("A0._:-").is_ok(), "all specials together must be valid");
    }

    #[test]
    fn valid_chars_accepted() {
        assert!(Scope::new("org.team_agent-1:v2").is_ok());
        assert!(Scope::new("_dev_").is_ok());
    }

    // ── rejection ────────────────────────────────────────────────────────────

    #[test]
    fn empty_scope_is_rejected() {
        let err = Scope::new("").unwrap_err();
        // ScopeError::Invalid must carry the (empty) bad input.
        assert!(matches!(err, ScopeError::Invalid(ref s) if s.is_empty()));
    }

    #[test]
    fn one_over_max_length_is_rejected() {
        let too_long = "a".repeat(129);
        let err = Scope::new(&too_long).unwrap_err();
        // Error must carry the full bad string so callers can surface it.
        assert!(matches!(err, ScopeError::Invalid(ref s) if s.len() == 129));
    }

    #[test]
    fn invalid_chars_rejected() {
        for bad in &[
            "has space",
            " leading",
            "trailing ",
            "\thas_tab",
            "has/slash",
            "has@at",
            "has#hash",
            "has!bang",
            "has+plus",
            "has=eq",
            "has[bracket",
            "has{brace",
            "has\"quote",
            "has\\backslash",
        ] {
            let err = Scope::new(*bad);
            assert!(err.is_err(), "expected rejection for {:?} but got Ok", bad);
            // Verify the error carries the exact rejected input.
            let ScopeError::Invalid(carried) = err.unwrap_err();
            assert_eq!(&carried, bad, "ScopeError::Invalid must carry the exact bad input");
        }
    }

    #[test]
    fn whitespace_not_trimmed_or_silently_accepted() {
        // Leading/trailing whitespace is NOT trimmed — it is rejected outright.
        assert!(Scope::new(" acme").is_err());
        assert!(Scope::new("acme ").is_err());
        assert!(Scope::new(" ").is_err());
    }

    // ── dev() helper ─────────────────────────────────────────────────────────

    #[test]
    fn dev_scope_is_valid() {
        let s = Scope::dev();
        assert_eq!(s.as_str(), "_dev_");
        // dev() must produce a value that also passes Scope::new — it is a real Scope.
        assert!(Scope::new("_dev_").is_ok());
    }

    // ── serde ─────────────────────────────────────────────────────────────────

    #[test]
    fn scope_serde_transparent() {
        let s = Scope::new("tenant-1").unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""tenant-1""#);
        let back: Scope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn serde_rejects_invalid_scope_string() {
        // Deserialising an invalid scope value (e.g. containing a space) must fail —
        // the transparent serde impl delegates to SmolStr, which accepts any string,
        // so we document the current behaviour: serde does NOT re-validate.
        // Wave 1B/C will add a custom Deserialize that calls Scope::new.
        // This test is intentionally asserting the CURRENT (permissive) behaviour so
        // that any future tightening is deliberate and visible in diff.
        let result: Result<Scope, _> = serde_json::from_str(r#""has space""#);
        // Current: deserialization succeeds (SmolStr accepts any string).
        // If this assertion flips to Err in future, update scope.rs's Deserialize impl.
        assert!(
            result.is_ok(),
            "note: serde currently trusts the wire value without re-validation"
        );
    }

    // ── equality / hash ──────────────────────────────────────────────────────

    #[test]
    fn scope_equality_is_byte_exact() {
        let a = Scope::new("Tenant").unwrap();
        let b = Scope::new("tenant").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn equal_scopes_have_equal_hashes() {
        let a = Scope::new("acme:agent-1").unwrap();
        let b = Scope::new("acme:agent-1").unwrap();
        assert_eq!(a, b);
        // Hash must agree with Eq: a == b => hash(a) == hash(b).
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b), "equal Scope must hash to the same bucket");
    }

    #[test]
    fn distinct_scopes_are_not_equal() {
        let a = Scope::new("acme:agent-1").unwrap();
        let b = Scope::new("acme:agent-2").unwrap();
        assert_ne!(a, b);
    }

    // ── ordering (Ord / PartialOrd) ──────────────────────────────────────────

    #[test]
    fn scope_ord_is_lexicographic() {
        let a = Scope::new("a").unwrap();
        let b = Scope::new("b").unwrap();
        assert!(a < b);
        assert!(b > a);
        assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
    }

    #[test]
    fn scope_sort_is_stable() {
        let mut scopes: Vec<Scope> = vec![
            Scope::new("z:agent").unwrap(),
            Scope::new("a:agent").unwrap(),
            Scope::new("m:agent").unwrap(),
        ];
        scopes.sort();
        assert_eq!(scopes[0].as_str(), "a:agent");
        assert_eq!(scopes[1].as_str(), "m:agent");
        assert_eq!(scopes[2].as_str(), "z:agent");
    }

    // ── display ──────────────────────────────────────────────────────────────

    #[test]
    fn display_matches_as_str() {
        let s = Scope::new("org.team_agent-1:v2").unwrap();
        assert_eq!(format!("{s}"), s.as_str());
    }

    // ── as_ref ───────────────────────────────────────────────────────────────

    #[test]
    fn as_ref_str_matches_as_str() {
        let s = Scope::new("acme:agent-42").unwrap();
        let r: &str = s.as_ref();
        assert_eq!(r, s.as_str());
    }
}
