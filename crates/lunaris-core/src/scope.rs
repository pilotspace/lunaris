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
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

    #[test]
    fn valid_scope_roundtrip() {
        let s = Scope::new("acme:agent-42").unwrap();
        assert_eq!(s.as_str(), "acme:agent-42");
    }

    #[test]
    fn empty_scope_is_rejected() {
        assert!(Scope::new("").is_err());
    }

    #[test]
    fn too_long_scope_is_rejected() {
        let long = "a".repeat(129);
        assert!(Scope::new(&long).is_err());
    }

    #[test]
    fn max_length_scope_is_accepted() {
        let at_limit = "a".repeat(128);
        assert!(Scope::new(&at_limit).is_ok());
    }

    #[test]
    fn invalid_char_rejected() {
        assert!(Scope::new("has space").is_err());
        assert!(Scope::new("has/slash").is_err());
        assert!(Scope::new("has@at").is_err());
    }

    #[test]
    fn valid_chars_accepted() {
        assert!(Scope::new("org.team_agent-1:v2").is_ok());
        assert!(Scope::new("_dev_").is_ok());
        assert!(Scope::new("A0._:-").is_ok());
    }

    #[test]
    fn dev_scope_is_valid() {
        let s = Scope::dev();
        assert_eq!(s.as_str(), "_dev_");
        // Must also pass the validation regex — dev() is a real Scope, just pre-computed.
        assert!(Scope::new("_dev_").is_ok());
    }

    #[test]
    fn scope_serde_transparent() {
        let s = Scope::new("tenant-1").unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""tenant-1""#);
        let back: Scope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn scope_equality_is_byte_exact() {
        let a = Scope::new("Tenant").unwrap();
        let b = Scope::new("tenant").unwrap();
        assert_ne!(a, b);
    }
}
