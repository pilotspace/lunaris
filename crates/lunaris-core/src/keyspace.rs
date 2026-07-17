//! Storage-agnostic primitive KV key helpers — RFC 0001 §3.6 (Wave 2.5B).
//!
//! These helpers encode the **canonical Lunaris key format** for every primitive
//! kind: `lunaris:{scope}:{kind}:{ulid}`. They are storage-backend–agnostic —
//! Moon, Postgres, and future backends all derive their per-row keys from these
//! functions so the format stays in one place.
//!
//! ## What lives here vs. in `lunaris-storage-moon::keyspace`
//!
//! | Here (core)                    | Moon keyspace (infra)              |
//! |--------------------------------|------------------------------------|
//! | `scope_prefix`                 | `ft_index_name` (FT.SEARCH name)   |
//! | `episode_key`, `chunk_key`, …  | `graph_key` (Moon GRAPH.QUERY name)|
//! | `*_prefix` scan helpers        | `mq_topic` (Moon MQ topic name)    |
//!
//! The Moon-specific names encode Moon's command vocabulary and stay in
//! `lunaris-storage-moon::keyspace`. The helpers here have no backend dependency
//! (they only import [`Scope`] and [`ulid::Ulid`]).
//!
//! ## Migration note (Wave 2.5B)
//!
//! Prior to Wave 2.5B, `lunaris-ingest` and `lunaris-retrieve` (engine layer)
//! imported these helpers from `lunaris-storage-moon::keyspace` (infra layer).
//! The dependency arrow pointed the wrong way. The helpers moved here; the Moon
//! crate re-exports them from this module for backwards compatibility within the
//! infra layer.

use ulid::Ulid;

use crate::scope::Scope;

// ---------------------------------------------------------------------------
// Scope prefix helper (single source of truth for `lunaris:{scope}:` prefix)
// ---------------------------------------------------------------------------

/// Returns the KV keyspace prefix for `scope`: `lunaris:{scope}:`.
///
/// All KV entries under a scope share this prefix, enabling per-scope
/// `SCAN MATCH <prefix>*` iteration without cross-scope bleed.
///
/// # Examples
///
/// ```
/// use lunaris_core::{Scope, keyspace::scope_prefix};
/// let scope = Scope::new("acme.agent-1").unwrap();
/// assert_eq!(scope_prefix(&scope), "lunaris:acme.agent-1:");
/// ```
#[inline]
pub fn scope_prefix(scope: &Scope) -> String {
    format!("lunaris:{}:", scope.as_str())
}

// ---------------------------------------------------------------------------
// Scoped primitive-key helpers
// ---------------------------------------------------------------------------

/// KV key for an episode: `lunaris:{scope}:episode:{ulid}`
///
/// # Examples
///
/// ```
/// use lunaris_core::{Scope, keyspace::episode_key};
/// use ulid::Ulid;
/// let scope = Scope::new("_dev_").unwrap();
/// let id = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
/// assert_eq!(episode_key(&scope, id), b"lunaris:_dev_:episode:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec());
/// ```
#[inline]
pub fn episode_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}episode:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for a chunk: `lunaris:{scope}:chunk:{ulid}`
#[inline]
pub fn chunk_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}chunk:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for an entity: `lunaris:{scope}:entity:{ulid}`
#[inline]
pub fn entity_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}entity:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for a relation: `lunaris:{scope}:relation:{ulid}`
#[inline]
pub fn relation_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}relation:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for a fact: `lunaris:{scope}:fact:{ulid}`
#[inline]
pub fn fact_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}fact:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for the `(subject, predicate)` secondary index used by cross-episode
/// contradiction detection: `lunaris:{scope}:factspo:{hex(subject_id)}:{predicate}`.
///
/// Unlike the primitive `*_key` helpers this is keyed by the **deterministic
/// 16-byte subject id** (raw bytes, NOT a `Ulid`) plus the predicate string, so
/// the structured-ingest write path can `read_as_of` every prior in-scope fact
/// asserting the same `(subject, predicate)` and classify the new fact as
/// NOOP / Append / Supersede. The subject id is taken as `&[u8; 16]` rather than
/// `lunaris_extract::EntityId` so `lunaris-core` keeps zero dependency on the
/// extractor crate (layering) — callers pass `&entity_id.0`.
///
/// # Examples
///
/// ```
/// use lunaris_core::{Scope, keyspace::fact_spo_key};
/// let scope = Scope::new("agent_a").unwrap();
/// let k = fact_spo_key(&scope, &[7u8; 16], "employer");
/// let s = String::from_utf8(k).unwrap();
/// assert!(s.starts_with("lunaris:agent_a:factspo:"));
/// assert!(s.ends_with(":employer"));
/// ```
#[inline]
pub fn fact_spo_key(scope: &Scope, subject_id: &[u8; 16], predicate: &str) -> Vec<u8> {
    // Lowercase-hex the subject id inline (lunaris-core has no `hex` dep).
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hexed = String::with_capacity(32);
    for &b in subject_id {
        hexed.push(HEX[(b >> 4) as usize] as char);
        hexed.push(HEX[(b & 0x0f) as usize] as char);
    }
    format!("{}factspo:{hexed}:{predicate}", scope_prefix(scope)).into_bytes()
}

/// KV key for a community: `lunaris:{scope}:community:{ulid}`
#[inline]
pub fn community_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}community:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for a document tree: `lunaris:{scope}:doctree:{ulid}`
///
/// One DocTree is persisted per Episode, keyed by the Episode's Ulid.
/// Phase 27 STRUCT-02.
#[inline]
pub fn doctree_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}doctree:{id}", scope_prefix(scope)).into_bytes()
}

/// KV key for a persistent per-memory activation-ledger record:
/// `lunaris:{scope}:activation:{ulid}` (ADD task activation-ledger).
///
/// `id` is the referenced MEMORY's ulid (chunk / fact / entity / episode —
/// whatever primitive the reference signal names), not a ledger-owned id:
/// there is exactly one activation record per `(scope, memory-ulid)`.
///
/// # Examples
///
/// ```
/// use lunaris_core::{Scope, keyspace::activation_key};
/// use ulid::Ulid;
/// let scope = Scope::new("_dev_").unwrap();
/// let id = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
/// assert_eq!(
///     activation_key(&scope, id),
///     b"lunaris:_dev_:activation:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
/// );
/// ```
#[inline]
pub fn activation_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}activation:{id}", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for activation-ledger records under `scope`:
/// `lunaris:{scope}:activation:`. Used by the ACT-R full-scope tick
/// ([`crate::activation`]'s consumers in `lunaris-consolidate`) and by the
/// batched read-time boost provider in `lunaris-retrieve` — `StoragePort`
/// has no native point-MGET primitive, so a single `scan_range` call over
/// this prefix is the one-round-trip primitive both use.
#[inline]
pub fn activation_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}activation:", scope_prefix(scope)).into_bytes()
}

/// KV key for an idempotency sidecar entry (HOOK-05):
/// `lunaris:{scope}:dedupe:{blake3_hex(raw)}`.
///
/// `raw` is arbitrary caller-supplied data (any opaque string) — it is
/// blake3-hashed rather than spliced so it can never alias the
/// `lunaris:{scope}:{kind}:{ulid}` format regardless of content
/// (ADD task moon-parity-honesty — closes the "SQLite-only idempotency"
/// v0.5 boundary for Moon).
#[inline]
pub fn dedupe_key(scope: &Scope, raw: &str) -> Vec<u8> {
    let hash = blake3::hash(raw.as_bytes());
    format!("{}dedupe:{}", scope_prefix(scope), hash.to_hex()).into_bytes()
}

// ---------------------------------------------------------------------------
// Scoped primitive scan-prefix helpers
// ---------------------------------------------------------------------------

/// Scan prefix for episodes under `scope`: `lunaris:{scope}:episode:`
#[inline]
pub fn episode_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}episode:", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for chunks under `scope`: `lunaris:{scope}:chunk:`
#[inline]
pub fn chunk_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}chunk:", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for entities under `scope`: `lunaris:{scope}:entity:`
#[inline]
pub fn entity_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}entity:", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for relations under `scope`: `lunaris:{scope}:relation:`
#[inline]
pub fn relation_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}relation:", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for facts under `scope`: `lunaris:{scope}:fact:`
#[inline]
pub fn fact_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}fact:", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for communities under `scope`: `lunaris:{scope}:community:`
#[inline]
pub fn community_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}community:", scope_prefix(scope)).into_bytes()
}

/// Scan prefix for document trees under `scope`: `lunaris:{scope}:doctree:`
#[inline]
pub fn doctree_prefix(scope: &Scope) -> Vec<u8> {
    format!("{}doctree:", scope_prefix(scope)).into_bytes()
}

// ---------------------------------------------------------------------------
// Reverse parse: extract a scope from a `lunaris:{scope}:{kind}:{ulid}` key
// ---------------------------------------------------------------------------

/// Parse the `{scope}` segment out of a Lunaris KV key of the form
/// `lunaris:{scope}:{kind}:{ulid}` (or any longer suffix).
///
/// Returns `Some(Scope)` if the key matches the canonical layout AND the
/// extracted segment is a valid [`Scope`]. Returns `None` for any non-Lunaris
/// key, malformed key, or scope segment that fails the `Scope::new` regex.
///
/// This is the inverse of [`scope_prefix`] + the primitive `*_key` helpers
/// and is used by [`StoragePort::list_scopes`](crate::StoragePort::list_scopes)
/// impls (Moon SCAN-parse, embedded SQLite key-scan) to recover the set of
/// known scopes from raw keys.
///
/// The input is `&[u8]` (not `&str`) because Moon's SCAN cursor returns raw
/// bytes — a malformed UTF-8 key short-circuits to `None` rather than
/// panicking. This is the only safe entry point for untrusted-byte keys.
///
/// # Examples
///
/// ```
/// use lunaris_core::{Scope, keyspace::parse_scope_from_key};
/// let k = b"lunaris:acme.agent-1:episode:01HZZZZZZZZZZZZZZZZZZZZZZZ";
/// let s = parse_scope_from_key(k).expect("valid scope");
/// assert_eq!(s.as_str(), "acme.agent-1");
///
/// // Non-Lunaris key: None.
/// assert!(parse_scope_from_key(b"otherprefix:xx:yy").is_none());
///
/// // Trailing colon but no kind: still parses the scope (the parser only
/// // requires a `:` after the scope segment so partial writes don't drop
/// // valid scopes from `list_scopes`).
/// assert_eq!(
///     parse_scope_from_key(b"lunaris:acme.agent-1:")
///         .map(|s| s.as_str().to_string()),
///     Some("acme.agent-1".to_string()),
/// );
/// // No `:` after `lunaris:{scope}` at all: None.
/// assert!(parse_scope_from_key(b"lunaris:acme.agent-1").is_none());
///
/// // Invalid UTF-8: None (never panics).
/// assert!(parse_scope_from_key(&[0xff, 0xfe, 0xfd]).is_none());
/// ```
pub fn parse_scope_from_key(raw: &[u8]) -> Option<Scope> {
    // Lunaris keys are ASCII-only by construction (scope regex bans non-ASCII,
    // primitive kinds are lowercase ASCII, ULIDs are Crockford base32). Reject
    // any non-UTF-8 input rather than try to recover.
    let s = std::str::from_utf8(raw).ok()?;
    let rest = s.strip_prefix("lunaris:")?;
    // The scope segment is everything up to (but not including) the next `:`.
    // The `:` MUST be present — a bare `lunaris:{scope}` with no trailing
    // `:{kind}:…` is not a primitive key and is rejected so callers do not
    // confuse "incomplete write" with "valid scope".
    let colon = rest.find(':')?;
    let scope_str = &rest[..colon];
    // Defensive: re-validate via `Scope::new` so a malformed key (e.g. an
    // empty scope segment from `lunaris::episode:…`) is rejected even though
    // the byte layout matched.
    Scope::new(scope_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_a() -> Scope {
        Scope::new("acme.agent-1").unwrap()
    }

    fn scope_b() -> Scope {
        Scope::new("acme.agent-2").unwrap()
    }

    #[test]
    fn scope_prefix_format() {
        let s = scope_a();
        assert_eq!(scope_prefix(&s), "lunaris:acme.agent-1:");
    }

    #[test]
    fn scoped_key_stable_format() {
        let scope = Scope::new("_dev_").unwrap();
        let id = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
        assert_eq!(
            episode_key(&scope, id),
            b"lunaris:_dev_:episode:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            chunk_key(&scope, id),
            b"lunaris:_dev_:chunk:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            entity_key(&scope, id),
            b"lunaris:_dev_:entity:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            relation_key(&scope, id),
            b"lunaris:_dev_:relation:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(fact_key(&scope, id), b"lunaris:_dev_:fact:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec());
        assert_eq!(
            community_key(&scope, id),
            b"lunaris:_dev_:community:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
    }

    #[test]
    fn scoped_keys_differ_across_scopes() {
        let id = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
        let ka = episode_key(&scope_a(), id);
        let kb = episode_key(&scope_b(), id);
        assert_ne!(ka, kb, "same ULID in different scopes must produce different keys");
        assert!(ka.starts_with(b"lunaris:acme.agent-1:episode:"));
        assert!(kb.starts_with(b"lunaris:acme.agent-2:episode:"));
    }

    /// RC-2 (v0.2.1) closure — the validation regex no longer permits `:`
    /// so the `lunaris:{scope}:{kind}:{ulid}` format is unambiguous at the
    /// type level. Any colon-containing scope is rejected by `Scope::new`,
    /// and any colon-free scope's `scope_prefix` cannot byte-alias any
    /// other scope's `*_prefix`.
    ///
    /// Before v0.2.1: `Scope::new("a:episode")` was Ok and
    /// `scope_prefix(&Scope("a:episode")) == episode_prefix(&Scope("a"))`.
    /// After v0.2.1: `Scope::new("a:episode")` is Err and the test below
    /// is the regression pin.
    #[test]
    fn scan_prefix_does_not_alias_across_kinds() {
        // The previously-dangerous form is now type-rejected.
        assert!(Scope::new("a:episode").is_err(), "colon-containing scope must be rejected");
        assert!(Scope::new("a:chunk").is_err());
        assert!(Scope::new("a:fact").is_err());

        // For any valid scope, scope_prefix and episode_prefix are
        // structurally distinct (different suffixes).
        let a = Scope::new("a").unwrap();
        let sp = scope_prefix(&a).into_bytes();
        let ep = episode_prefix(&a);
        assert_ne!(sp, ep, "scope_prefix and episode_prefix must differ for any scope");
        // The episode prefix is the scope prefix + "episode:".
        assert!(ep.starts_with(&sp));
        assert!(ep.ends_with(b"episode:"));
    }

    #[test]
    fn prefix_matches_key_starts() {
        let scope = Scope::new("org.team").unwrap();
        let id = Ulid::new();
        assert!(episode_key(&scope, id).starts_with(&episode_prefix(&scope)));
        assert!(chunk_key(&scope, id).starts_with(&chunk_prefix(&scope)));
        assert!(entity_key(&scope, id).starts_with(&entity_prefix(&scope)));
        assert!(relation_key(&scope, id).starts_with(&relation_prefix(&scope)));
        assert!(fact_key(&scope, id).starts_with(&fact_prefix(&scope)));
        assert!(community_key(&scope, id).starts_with(&community_prefix(&scope)));
    }

    // ---- parse_scope_from_key (inverse of scope_prefix, used by list_scopes) ----

    #[test]
    fn parse_scope_roundtrips_every_primitive_kind() {
        let scope = Scope::new("acme.agent-42").unwrap();
        let id = Ulid::new();
        for k in [
            episode_key(&scope, id),
            chunk_key(&scope, id),
            entity_key(&scope, id),
            relation_key(&scope, id),
            fact_key(&scope, id),
            community_key(&scope, id),
        ] {
            let parsed = parse_scope_from_key(&k).expect("valid Lunaris key must parse");
            assert_eq!(parsed.as_str(), scope.as_str(), "key {k:?} parsed to wrong scope");
        }
    }

    #[test]
    fn parse_scope_rejects_non_lunaris_prefix() {
        assert!(parse_scope_from_key(b"other:scope:kind:id").is_none());
        assert!(parse_scope_from_key(b"").is_none());
        // Looks like a prefix but no scope separator.
        assert!(parse_scope_from_key(b"lunaris:no-trailing-colon").is_none());
        // Trailing colon but no kind segment yet.
        assert!(
            parse_scope_from_key(b"lunaris:scope:").map(|s| s.as_str().to_string())
                == Some("scope".to_string())
        );
    }

    #[test]
    fn parse_scope_rejects_empty_segment() {
        // `lunaris::kind:id` — empty scope segment must not produce a Scope.
        assert!(parse_scope_from_key(b"lunaris::episode:01HZZZ").is_none());
    }

    #[test]
    fn parse_scope_rejects_invalid_utf8() {
        // 0xFF is not valid UTF-8; must return None, never panic.
        assert!(parse_scope_from_key(&[0xff, 0xfe, b':']).is_none());
    }

    #[test]
    fn parse_scope_rejects_scope_with_invalid_chars() {
        // Hand-craft a key where the scope segment fails Scope::new's regex
        // (e.g. contains `/` which is disallowed). Must return None — the
        // parser is the LAST defence and re-validates via Scope::new.
        assert!(parse_scope_from_key(b"lunaris:bad/scope:episode:01HZZZ").is_none());
    }
}
