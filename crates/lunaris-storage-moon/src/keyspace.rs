//! Moon keyspace conventions for Lunaris primitives — RFC 0001 §3.6 (Wave 1C).
//!
//! ## What lives here vs. `lunaris_core::keyspace` (Wave 2.5B)
//!
//! | This module (Moon-specific)  | `lunaris_core::keyspace` (storage-agnostic) |
//! |------------------------------|----------------------------------------------|
//! | `ft_index_name`              | `scope_prefix`                               |
//! | `graph_key`                  | `episode_key`, `chunk_key`, …               |
//! | `mq_topic`                   | `*_prefix` scan helpers                      |
//!
//! The primitive KV key helpers (`episode_key`, `chunk_key`, etc.) were moved to
//! `lunaris_core::keyspace` in Wave 2.5B so the engine layer (`lunaris-ingest`,
//! `lunaris-retrieve`) can import them without depending on this infra crate.
//! They are re-exported here for backwards compatibility within this crate and
//! for any `lunaris_storage_moon::keyspace::*` import paths in integration tests.
//!
//! ## FT.SEARCH key-decode contract (preserved)
//!
//! `atomic.rs::VectorUpsert` writes hash entries with key
//! `{ft_index_name(scope, kind)}:{id_hex}`.
//! `vector.rs::decode_key(raw, ft_idx)` strips `<ft_idx>:` and hex-decodes
//! to a 16-byte ULID — this contract is unchanged; only the index name
//! (prefix) changes per scope.

use lunaris_core::Scope;

// Wave 2.5B: primitive KV key helpers now live in lunaris-core::keyspace.
// Re-export here so any code within this crate (and any existing
// `lunaris_storage_moon::keyspace::*` import paths in tests) keeps compiling.
pub use lunaris_core::keyspace::{
    chunk_key, chunk_prefix, community_key, community_prefix, entity_key, entity_prefix,
    episode_key, episode_prefix, fact_key, fact_prefix, relation_key, relation_prefix,
    scope_prefix,
};

// ---------------------------------------------------------------------------
// Moon-specific scope-routing helpers (RFC 0001 §3.6)
// ---------------------------------------------------------------------------

/// Returns the FT index name for a `scope + kind` pair: `lunaris_{scope}_{kind}_idx`.
///
/// # Parameters
///
/// - `scope` — the agent/tenant scope (e.g. `"acme:agent-42"`)
/// - `kind`  — the index kind (e.g. `"chunks"`, `"entities"`, `"facts"`, `"communities"`)
///
/// # Examples
///
/// ```
/// use lunaris_core::Scope;
/// use lunaris_storage_moon::keyspace::ft_index_name;
/// let scope = Scope::new("acme:agent-42").unwrap();
/// assert_eq!(ft_index_name(&scope, "chunks"), "lunaris_acme:agent-42_chunks_idx");
/// ```
#[inline]
pub fn ft_index_name(scope: &Scope, kind: &str) -> String {
    format!("lunaris_{}_{}_{}", scope.as_str(), kind, "idx")
}

/// Returns the Moon graph key for `scope`: `lunaris_{scope}_graph`.
///
/// Moon's `GRAPH.QUERY` operates on a named graph. Each scope gets its own
/// graph — cross-scope graph queries are disallowed by construction (RFC 0001 §2.3).
#[inline]
pub fn graph_key(scope: &Scope) -> String {
    format!("lunaris_{}_graph", scope.as_str())
}

/// Returns the scoped MQ topic name: `lunaris:{scope}:{name}`.
///
/// Each consolidation / verification worker consumes its own per-scope topic
/// so a hot scope cannot starve a cold one (RFC 0001 §3.7).
#[inline]
pub fn mq_topic(scope: &Scope, name: &str) -> String {
    format!("lunaris:{}:{}", scope.as_str(), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::Scope;
    use ulid::Ulid;

    fn scope_a() -> Scope {
        Scope::new("acme:agent-1").unwrap()
    }

    fn scope_b() -> Scope {
        Scope::new("acme:agent-2").unwrap()
    }

    // ---- RFC 0001 §3.6 routing-helper contract tests (Moon-specific) ----

    #[test]
    fn ft_index_name_format() {
        let s = scope_a();
        assert_eq!(ft_index_name(&s, "chunks"), "lunaris_acme:agent-1_chunks_idx");
        assert_eq!(ft_index_name(&s, "entities"), "lunaris_acme:agent-1_entities_idx");
        assert_eq!(ft_index_name(&s, "facts"), "lunaris_acme:agent-1_facts_idx");
        assert_eq!(ft_index_name(&s, "communities"), "lunaris_acme:agent-1_communities_idx");
    }

    #[test]
    fn graph_key_format() {
        let s = scope_a();
        assert_eq!(graph_key(&s), "lunaris_acme:agent-1_graph");
    }

    #[test]
    fn mq_topic_format() {
        let s = scope_a();
        assert_eq!(mq_topic(&s, "consolidate"), "lunaris:acme:agent-1:consolidate");
        assert_eq!(mq_topic(&s, "verify"), "lunaris:acme:agent-1:verify");
    }

    // ---- FT.SEARCH key-decode contract (preserved from pre-RFC baseline) ----
    //
    // `atomic.rs::VectorUpsert` writes `{ft_index_name(scope, kind)}:{hex32}`.
    // `vector.rs::decode_key(raw, ft_idx)` strips `<ft_idx>:` and hex-decodes
    // to a 16-byte ULID. This test pins the end-to-end round-trip so any
    // regression is caught before it silently breaks the hydrate path.

    #[test]
    fn ft_key_roundtrip_decode_contract() {
        let scope = scope_a();
        let ulid_bytes: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let hex = hex::encode(ulid_bytes);
        let ft_idx = ft_index_name(&scope, "chunks");

        // Simulate the key that atomic.rs writes for VectorUpsert.
        let written_key = format!("{ft_idx}:{hex}");

        // Simulate decode_key in vector.rs stripping ft_idx prefix + hex-decoding.
        let prefix_len = ft_idx.len() + 1; // +1 for the ':'
        assert!(written_key.len() > prefix_len, "key must be longer than prefix");
        assert!(written_key.starts_with(&ft_idx), "key must start with ft_idx");
        assert_eq!(written_key.as_bytes()[ft_idx.len()], b':', "colon separator");
        let decoded = hex::decode(&written_key[prefix_len..]).expect("valid hex");
        assert_eq!(decoded, ulid_bytes.to_vec(), "decoded ULID bytes must match original");
        assert_eq!(decoded.len(), 16, "ULID must be exactly 16 bytes");
    }

    // ---- Re-exported primitive key helpers (from lunaris-core::keyspace) ----

    #[test]
    fn scope_prefix_re_export_format() {
        let s = scope_a();
        assert_eq!(scope_prefix(&s), "lunaris:acme:agent-1:");
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
    }

    #[test]
    fn ft_index_names_differ_across_scopes_same_kind() {
        let idx_a = ft_index_name(&scope_a(), "chunks");
        let idx_b = ft_index_name(&scope_b(), "chunks");
        assert_ne!(idx_a, idx_b);
    }

    #[test]
    fn graph_keys_differ_across_scopes() {
        assert_ne!(graph_key(&scope_a()), graph_key(&scope_b()));
    }

    #[test]
    fn mq_topics_differ_across_scopes_same_name() {
        let ta = mq_topic(&scope_a(), "consolidate");
        let tb = mq_topic(&scope_b(), "consolidate");
        assert_ne!(ta, tb);
    }
}
