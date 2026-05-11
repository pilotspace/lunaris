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
/// let scope = Scope::new("acme:agent-1").unwrap();
/// assert_eq!(scope_prefix(&scope), "lunaris:acme:agent-1:");
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

/// KV key for a community: `lunaris:{scope}:community:{ulid}`
#[inline]
pub fn community_key(scope: &Scope, id: Ulid) -> Vec<u8> {
    format!("{}community:{id}", scope_prefix(scope)).into_bytes()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_a() -> Scope {
        Scope::new("acme:agent-1").unwrap()
    }

    fn scope_b() -> Scope {
        Scope::new("acme:agent-2").unwrap()
    }

    #[test]
    fn scope_prefix_format() {
        let s = scope_a();
        assert_eq!(scope_prefix(&s), "lunaris:acme:agent-1:");
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
        assert!(ka.starts_with(b"lunaris:acme:agent-1:episode:"));
        assert!(kb.starts_with(b"lunaris:acme:agent-2:episode:"));
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
}
