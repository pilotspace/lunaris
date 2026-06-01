//! Six bi-temporal primitives — verbatim per blueprint §3.3.
//!
//! Every primitive carries a `BiTemporal { valid, sys }` stamp from a shared `HlcClock`.
//! Every primitive is `Send + Sync + 'static`, `Debug`, `Clone`, `PartialEq`, and serde-roundtrippable.
//!
//! RFC 0001 (v0.2): every primitive now carries `pub scope: Scope` as a first-class
//! partition key for multi-agent / multi-tenant isolation. Constructors take `scope`
//! as the first argument. Existing call sites use `Scope::dev()` during the Wave 0
//! migration; Wave 1 replaces those with real per-agent scopes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::bitemporal::BiTemporal;
use crate::hlc::HlcClock;
use crate::scope::Scope;

// ---------------- Episode ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub id: Ulid,
    /// RFC 0001 — partition key for multi-agent / multi-tenant isolation.
    pub scope: Scope,
    pub source: String,
    pub content: String,
    pub t_ref: Option<DateTime<Utc>>,
    pub bt: BiTemporal,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl Episode {
    /// Construct a new [`Episode`].
    ///
    /// `scope` is the partition key (RFC 0001). Use [`Scope::dev()`] at Wave 0
    /// call sites where the real scope has not yet been threaded through.
    pub fn new(
        scope: Scope,
        source: impl Into<String>,
        content: impl Into<String>,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            scope,
            source: source.into(),
            content: content.into(),
            t_ref: None,
            bt: BiTemporal::now(clock),
            metadata: serde_json::Map::new(),
        }
    }
}

// ---------------- Chunk ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Ulid,
    /// RFC 0001 — partition key, inherited from the parent [`Episode`].
    pub scope: Scope,
    pub episode_id: Ulid,
    pub text: String,
    pub tokens: u32,
    pub offset: u32,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub overlap_tail: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    /// Optional link to the nearest parent [`TocNode`] in the document tree.
    ///
    /// `None` in Phase 27 (field + migration + serde-compat delivered here;
    /// full parent wiring — setting a non-None value — lands in Phase 29).
    /// Pre-existing rows serialised without this field deserialise to `None`
    /// via `#[serde(default)]` (STRUCT-03 serde back-compat contract).
    ///
    /// Moon backend: field travels in the existing JSONB KvPut payload — no DDL.
    /// Postgres backend: `chunks.parent_id BYTEA NULL` (migration 20260601000008).
    #[serde(default)]
    pub parent_id: Option<Ulid>,
    pub bt: BiTemporal,
}

impl Chunk {
    /// Construct a new [`Chunk`].
    ///
    /// `scope` must match the parent [`Episode::scope`] (RFC 0001 §3.2).
    pub fn new(
        scope: Scope,
        episode_id: Ulid,
        text: impl Into<String>,
        tokens: u32,
        offset: u32,
        heading_path: Vec<String>,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            scope,
            episode_id,
            text: text.into(),
            tokens,
            offset,
            heading_path,
            overlap_tail: String::new(),
            embedding: None,
            parent_id: None,
            bt: BiTemporal::now(clock),
        }
    }
}

// ---------------- Entity ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: Ulid,
    /// RFC 0001 — partition key for multi-agent / multi-tenant isolation.
    pub scope: Scope,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub entity_type: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
    pub confidence: f32,
}

impl Entity {
    /// Construct a new [`Entity`].
    ///
    /// `scope` is the partition key (RFC 0001). `src` and `dst` of any
    /// [`Relation`] referencing this entity MUST share the same scope —
    /// cross-scope relations are disallowed by construction in v0.2.
    pub fn new(
        scope: Scope,
        name: impl Into<String>,
        entity_type: impl Into<String>,
        confidence: f32,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            scope,
            name: name.into(),
            aliases: Vec::new(),
            entity_type: entity_type.into(),
            embedding: None,
            bt: BiTemporal::now(clock),
            confidence,
        }
    }
}

// ---------------- Relation ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub id: Ulid,
    /// RFC 0001 — partition key. `src` and `dst` MUST resolve within this scope.
    pub scope: Scope,
    pub src: Ulid,
    pub dst: Ulid,
    pub rel_type: String,
    pub bt: BiTemporal,
    pub confidence: f32,
    #[serde(default)]
    pub provenance: Vec<Ulid>,
}

impl Relation {
    /// Construct a new [`Relation`].
    ///
    /// `src` and `dst` MUST resolve within `scope` — cross-scope graph
    /// references are disallowed by construction in v0.2 (RFC 0001 §2.3).
    pub fn new(
        scope: Scope,
        src: Ulid,
        dst: Ulid,
        rel_type: impl Into<String>,
        confidence: f32,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            scope,
            src,
            dst,
            rel_type: rel_type.into(),
            bt: BiTemporal::now(clock),
            confidence,
            provenance: Vec::new(),
        }
    }
}

// ---------------- Fact ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: Ulid,
    /// RFC 0001 — partition key for multi-agent / multi-tenant isolation.
    pub scope: Scope,
    pub subject: Ulid,
    pub predicate: String,
    pub object: Ulid,
    pub fact_text: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
    pub confidence: f32,
    #[serde(default)]
    pub provenance: Vec<Ulid>,
    pub activation: f32,
}

impl Fact {
    /// Construct a new [`Fact`].
    ///
    /// `scope` is the partition key (RFC 0001). Corresponds to "Claim" in the
    /// RFC §3.2 primitive list (the codebase uses `Fact` as the canonical name).
    pub fn new(
        scope: Scope,
        subject: Ulid,
        predicate: impl Into<String>,
        object: Ulid,
        fact_text: impl Into<String>,
        confidence: f32,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            scope,
            subject,
            predicate: predicate.into(),
            object,
            fact_text: fact_text.into(),
            embedding: None,
            bt: BiTemporal::now(clock),
            confidence,
            provenance: Vec::new(),
            activation: 0.0,
        }
    }
}

// ---------------- Community ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Community {
    pub id: Ulid,
    /// RFC 0001 — partition key. Corresponds to "Source" in the RFC §3.2
    /// primitive list (the codebase uses `Community` as the canonical name).
    pub scope: Scope,
    pub level: u8,
    pub parent: Option<Ulid>,
    #[serde(default)]
    pub members: Vec<Ulid>,
    pub summary: String,
    #[serde(default)]
    pub summary_embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
}

impl Community {
    /// Construct a new [`Community`].
    ///
    /// `scope` is the partition key (RFC 0001). All member entity IDs in
    /// `members` MUST resolve within this scope.
    pub fn new(scope: Scope, level: u8, summary: impl Into<String>, clock: &HlcClock) -> Self {
        Self {
            id: Ulid::new(),
            scope,
            level,
            parent: None,
            members: Vec::new(),
            summary: summary.into(),
            summary_embedding: None,
            bt: BiTemporal::now(clock),
        }
    }
}

// ---------------------------------------------------------------------------
// STRUCT-03 tests — Chunk.parent_id serde back-compat + construction
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlc::HlcClock;
    use crate::scope::Scope;

    fn dev_scope() -> Scope {
        Scope::dev()
    }

    fn test_clock() -> std::sync::Arc<HlcClock> {
        HlcClock::new(0)
    }

    #[test]
    fn chunk_deserializes_without_parent_id() {
        // Prove serde back-compat: a JSON row serialized without "parent_id"
        // (pre-Phase 27 row) must deserialize successfully with parent_id=None.
        // Strategy: serialize a real Chunk, remove "parent_id" from the JSON
        // object, then deserialize — format-agnostic, no hardcoded BiTemporal layout.
        let clock = test_clock();
        let chunk = Chunk::new(dev_scope(), ulid::Ulid::new(), "hello world", 2, 0, vec![], &clock);
        let mut map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&serde_json::to_string(&chunk).unwrap()).unwrap();
        // Simulate a pre-Phase-27 row by dropping the parent_id field entirely.
        map.remove("parent_id");
        let stripped = serde_json::to_string(&map).unwrap();
        let back: Chunk = serde_json::from_str(&stripped)
            .expect("back-compat: must deserialize without parent_id");
        assert!(chunk.parent_id.is_none(), "parent_id must be None for pre-existing rows");
        assert_eq!(back.text, chunk.text);
    }

    #[test]
    fn chunk_deserializes_with_parent_id() {
        // Prove that a row WITH parent_id set deserializes correctly.
        let clock = test_clock();
        let mut chunk =
            Chunk::new(dev_scope(), ulid::Ulid::new(), "hello world", 2, 0, vec![], &clock);
        let parent = ulid::Ulid::new();
        chunk.parent_id = Some(parent);
        let json = serde_json::to_string(&chunk).unwrap();
        let back: Chunk = serde_json::from_str(&json).expect("must deserialize with parent_id");
        assert_eq!(back.parent_id, Some(parent), "parent_id must be Some when present in JSON");
    }

    #[test]
    fn chunk_new_has_none_parent_id() {
        let clock = test_clock();
        let chunk = Chunk::new(dev_scope(), ulid::Ulid::new(), "test text", 3, 0, vec![], &clock);
        assert!(chunk.parent_id.is_none(), "Chunk::new must produce parent_id = None");
    }

    #[test]
    fn chunk_with_parent_id_roundtrips_serde() {
        let clock = test_clock();
        let mut chunk = Chunk::new(
            dev_scope(),
            ulid::Ulid::new(),
            "test text",
            3,
            0,
            vec!["Section 1".to_string()],
            &clock,
        );
        let parent = ulid::Ulid::new();
        chunk.parent_id = Some(parent);

        let json = serde_json::to_string(&chunk).expect("serialize");
        let back: Chunk = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.parent_id, Some(parent));
        assert_eq!(back.text, chunk.text);
    }
}
