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
//! The string must match `^[A-Za-z0-9_\-.]{1,128}$` (enforced by
//! [`Scope::new`]). The unchecked constructor `Scope::from_trusted` is
//! `pub(crate)` and is only used by trusted internal call sites (e.g.,
//! deserialization of previously-validated wire data).
//!
//! ## Examples
//!
//! ```
//! use lunaris_core::Scope;
//! let s = Scope::new("acme.agent-42").unwrap();
//! assert_eq!(s.as_str(), "acme.agent-42");
//! ```

use smol_str::SmolStr;
use thiserror::Error;

/// Validation regex fragment — kept as a const so backends and tests can
/// re-use it without duplicating the pattern.
///
/// Pattern: `^[A-Za-z0-9_\-.]{1,128}$`
///
/// RC-2 (v0.2.1): `:` was removed from the allowed alphabet to close the
/// SCAN prefix delimiter ambiguity. The KV key format
/// `lunaris:{scope}:{kind}:{ulid}` uses `:` as the field separator, so a
/// scope like `"a:episode"` previously produced byte-identical bytes to
/// `episode_prefix(&Scope("a"))` and `SCAN MATCH <prefix>*` under the
/// colliding scope on Moon could enumerate the other scope's episodes.
/// Dropping `:` makes the format unambiguous at the type level.
///
/// **Breaking change** for any v0.2.0 deployment that minted scope strings
/// containing `:` (e.g. `acme:agent-42`). The recommended replacement is
/// `.` or `-` (`acme.agent-42`). Enforcement is at the type level: `Scope::new`
/// is the only constructor, and the hand-rolled `Deserialize` re-runs it on
/// wire bytes. (v0.2.1 also tightened a matching Postgres CHECK constraint;
/// that backend was removed in 0.7.0.)
const VALID_SCOPE_CHARS: fn(char) -> bool =
    |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
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
/// The string must match `^[A-Za-z0-9_\-.]{1,128}$`. This is enforced by
/// [`Scope::new`]; the unchecked constructor is `pub(crate)` and only used by
/// trusted internal call sites (deserialization of validated wire data).
///
/// # Examples
///
/// ```
/// use lunaris_core::Scope;
/// let s = Scope::new("acme.agent-42").unwrap();
/// assert_eq!(s.as_str(), "acme.agent-42");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct Scope(SmolStr);

/// RC-4 (v0.2 release-gate review): re-validate on the wire boundary.
///
/// The previous derived `Deserialize` with `#[serde(transparent)]` accepted
/// any string, bypassing [`Scope::new`]'s regex. Internal deserialization
/// sites (rows fetched from a future cloud-API backend, MQ envelopes that
/// gain a `scope` field, etc.) would have trusted attacker-controlled bytes.
/// This impl forces every wire-side `Scope` to clear [`Scope::new`].
impl<'de> serde::Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = SmolStr::deserialize(d)?;
        Scope::new(s.as_str()).map_err(serde::de::Error::custom)
    }
}

impl Scope {
    /// Construct a `Scope` from `s`, enforcing the validation regex
    /// `^[A-Za-z0-9_\-.]{1,128}$`.
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
    /// the invariant `^[A-Za-z0-9_\-.]{1,128}$` holds.
    ///
    /// Used when deserializing scope values that came back out of storage —
    /// they were validated by `Scope::new` on the way in.
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
        // SAFETY: "_dev_" matches ^[A-Za-z0-9_\-.]{1,128}$ by inspection.
        Self(SmolStr::new("_dev_"))
    }

    /// Is `segment` a legal sub-partition segment?
    ///
    /// A segment is a non-empty run of `[A-Za-z0-9_-]`. Note `.` is EXCLUDED
    /// here even though [`Scope::new`] permits it in a whole scope, because
    /// `.` is the level separator (see [`child`](Scope::child)): a segment
    /// carrying its own `.` would forge an extra level. `:` and `/` are
    /// likewise excluded so a composed segment can never byte-alias the
    /// `lunaris:{scope}:{kind}:{ulid}` KV format.
    #[inline]
    pub fn is_valid_segment(segment: &str) -> bool {
        !segment.is_empty()
            && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    /// Compose a child sub-partition by appending a validated `.{segment}`.
    ///
    /// `segment` must satisfy [`is_valid_segment`](Scope::is_valid_segment);
    /// the full composed string is then re-validated by [`Scope::new`]
    /// (alphabet + 128-byte cap). Consequently `self.as_str()` is ALWAYS a
    /// byte-prefix of the returned child — this is the load-bearing isolation
    /// guarantee for multi-level memory (RFC 0001 sub-partitions): a caller
    /// can only NARROW into a sub-partition of its own scope, never escape to
    /// a sibling or parent.
    pub fn child(&self, segment: &str) -> Result<Scope, ScopeError> {
        if !Self::is_valid_segment(segment) {
            return Err(ScopeError::Invalid(segment.to_string()));
        }
        Scope::new(format!("{}.{segment}", self.0.as_str()))
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

/// The canonical memory-partition levels, composed UNDER the JWT base scope
/// in this fixed order (`User` → `Agent` → `Session`). Each carries a
/// one-char disambiguating tag so the composed scope is self-describing and
/// collision-resistant: `{base}.u-{user}.a-{agent}.s-{session}`, including
/// only the levels whose id is present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryLevel {
    /// Per-user memory space (tag `u`).
    User,
    /// Per-agent memory space (tag `a`).
    Agent,
    /// Per-session / run memory space (tag `s`).
    Session,
}

impl MemoryLevel {
    /// The one-char tag prefixed to a level id in the composed scope.
    #[inline]
    pub fn tag(&self) -> &'static str {
        match self {
            MemoryLevel::User => "u",
            MemoryLevel::Agent => "a",
            MemoryLevel::Session => "s",
        }
    }
}

/// Compose the JWT-bound `base` scope with optional user/agent/session ids,
/// in the canonical [`MemoryLevel`] order, into the bound partition.
///
/// Each present id becomes a `{tag}-{id}` segment appended via
/// [`Scope::child`], so `base.as_str()` is ALWAYS a byte-prefix of the
/// result (sub-partition, never escape). All-`None` returns `base.clone()`
/// — back-compat: operate at the base scope, today's behavior.
///
/// An id outside the segment alphabet (`[A-Za-z0-9_-]`, e.g. one carrying a
/// `.`/`:`/`/`) or one whose composition exceeds the 128-byte scope cap
/// yields `Err(ScopeError::Invalid)`. The HTTP layer pre-screens ids with
/// [`Scope::is_valid_segment`] so it can distinguish `invalid_level_segment`
/// from `scope_too_long` (only the length cap can fail once ids are clean).
pub fn compose_levels(
    base: &Scope,
    user: Option<&str>,
    agent: Option<&str>,
    session: Option<&str>,
) -> Result<Scope, ScopeError> {
    let mut scope = base.clone();
    for (level, id) in
        [(MemoryLevel::User, user), (MemoryLevel::Agent, agent), (MemoryLevel::Session, session)]
    {
        if let Some(id) = id {
            scope = scope.child(&format!("{}-{id}", level.tag()))?;
        }
    }
    Ok(scope)
}

/// Error returned when constructing an invalid [`Scope`].
#[derive(Debug, Error)]
pub enum ScopeError {
    /// The string is empty, too long (> 128 chars), or contains a character
    /// outside `[A-Za-z0-9_\-.]`.
    #[error("scope must be 1..=128 chars of [A-Za-z0-9_\\-.]; got {0:?}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── valid construction ────────────────────────────────────────────────────

    #[test]
    fn valid_scope_roundtrip() {
        let s = Scope::new("acme.agent-42").unwrap();
        assert_eq!(s.as_str(), "acme.agent-42");
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
        assert!(Scope::new("do.t").is_ok(), "dot must be valid");
        // Combined in one identifier — same as the pattern A0._-
        assert!(Scope::new("A0._.-").is_ok(), "all specials together must be valid");
    }

    /// RC-2 (v0.2.1): `:` is no longer in the allowed alphabet. This test
    /// pins the breaking-change boundary so any future relaxation that
    /// re-adds `:` will fail loudly here AND in the doc comment.
    #[test]
    fn colon_is_rejected() {
        assert!(Scope::new("co:lon").is_err(), "colon MUST be rejected post-v0.2.1");
        assert!(
            Scope::new("a:episode").is_err(),
            "the SCAN-aliasing scope form `a:episode` MUST be rejected at the type level"
        );
        assert!(Scope::new("tenant:1").is_err());
        // Bare colon at the end / start.
        assert!(Scope::new(":lead").is_err());
        assert!(Scope::new("trail:").is_err());
    }

    #[test]
    fn valid_chars_accepted() {
        assert!(Scope::new("org.team_agent-1.v2").is_ok());
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
            // RC-2 (v0.2.1) — colon is no longer in the allowed alphabet.
            "has:colon",
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
        // RC-4 (v0.2 release-gate): custom Deserialize now re-validates against
        // Scope::new. Any wire string that fails the regex must fail deserialize.
        let result: Result<Scope, _> = serde_json::from_str(r#""has space""#);
        assert!(result.is_err(), "invalid scope must be rejected at deserialize");

        // Sanity: a valid wire string still round-trips.
        let ok: Scope = serde_json::from_str(r#""acme.agent-1""#).unwrap();
        assert_eq!(ok.as_str(), "acme.agent-1");

        // RC-2: a wire string with `:` is now rejected at deserialize
        // (regression-pin for the v0.2.1 regex tightening).
        let colon: Result<Scope, _> = serde_json::from_str(r#""acme:agent-1""#);
        assert!(colon.is_err(), "post-v0.2.1: colon must be rejected on the wire too");

        // And a too-long string is rejected.
        let too_long = format!("\"{}\"", "a".repeat(129));
        let bad: Result<Scope, _> = serde_json::from_str(&too_long);
        assert!(bad.is_err(), "129-char scope must be rejected at deserialize");
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
        let a = Scope::new("acme.agent-1").unwrap();
        let b = Scope::new("acme.agent-1").unwrap();
        assert_eq!(a, b);
        // Hash must agree with Eq: a == b => hash(a) == hash(b).
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b), "equal Scope must hash to the same bucket");
    }

    #[test]
    fn distinct_scopes_are_not_equal() {
        let a = Scope::new("acme.agent-1").unwrap();
        let b = Scope::new("acme.agent-2").unwrap();
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
            Scope::new("z.agent").unwrap(),
            Scope::new("a.agent").unwrap(),
            Scope::new("m.agent").unwrap(),
        ];
        scopes.sort();
        assert_eq!(scopes[0].as_str(), "a.agent");
        assert_eq!(scopes[1].as_str(), "m.agent");
        assert_eq!(scopes[2].as_str(), "z.agent");
    }

    // ── display ──────────────────────────────────────────────────────────────

    #[test]
    fn display_matches_as_str() {
        let s = Scope::new("org.team_agent-1.v2").unwrap();
        assert_eq!(format!("{s}"), s.as_str());
    }

    // ── as_ref ───────────────────────────────────────────────────────────────

    #[test]
    fn as_ref_str_matches_as_str() {
        let s = Scope::new("acme.agent-42").unwrap();
        let r: &str = s.as_ref();
        assert_eq!(r, s.as_str());
    }
}
