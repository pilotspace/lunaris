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

// ---------------- EpisodeBuilder ----------------

/// Scope-less payload builder for [`Episode`].
///
/// Callers assemble all Episode fields EXCEPT scope using this builder.
/// Scope is injected exactly once — by [`ScopedLunaris::ingest`] — via the
/// `pub(crate)` [`EpisodeBuilder::into_episode`] method. This makes it
/// impossible to construct an [`Episode`] with an arbitrary scope by reaching
/// around the `ScopedLunaris` wrapper.
///
/// # Example
///
/// ```ignore
/// let builder = EpisodeBuilder::new("agent:fs/report.md", "# Q3 Report\n...")
///     .t_ref(chrono::Utc::now());
/// // scope is injected by engine.scoped(scope_a).ingest(builder).await?
/// ```
#[derive(Clone, Debug)]
pub struct EpisodeBuilder {
    /// The `source` field of the resulting `Episode` (e.g. `"helios:fs/report.md"`).
    pub source: String,
    /// Raw content that will be chunked + embedded by the ingest pipeline.
    pub content: String,
    /// Optional real-world reference timestamp (valid time). When `None`,
    /// the ingest pipeline stamps the Episode with the current HLC.
    pub t_ref: Option<chrono::DateTime<chrono::Utc>>,
    /// Caller-supplied metadata key/value pairs.
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl EpisodeBuilder {
    /// Construct a new builder with the required `source` and `content`.
    ///
    /// `source` is the namespace-qualified origin identifier
    /// (e.g. `"helios:fs/report.md"` or `"chat:session-42/turn-7"`).
    /// `content` is the raw text that will be chunked + embedded.
    pub fn new(source: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            content: content.into(),
            t_ref: None,
            metadata: serde_json::Map::new(),
        }
    }

    /// Set the reference timestamp (valid time anchor).
    ///
    /// When not set, the ingest pipeline uses the current wall time from the
    /// [`HlcClock`] bound to the engine.
    pub fn t_ref(mut self, t: chrono::DateTime<chrono::Utc>) -> Self {
        self.t_ref = Some(t);
        self
    }

    /// Merge `metadata` key/value pairs into the builder.
    pub fn metadata(mut self, m: serde_json::Map<String, serde_json::Value>) -> Self {
        self.metadata.extend(m);
        self
    }

    /// Materialise the builder into an [`Episode`].
    ///
    /// `scope` can ONLY be provided by [`lunaris::handle::ScopedLunaris::ingest`]
    /// — callers outside the `lunaris` crate cannot call this method, so
    /// they cannot set an arbitrary scope on an Episode.
    ///
    /// The `clock` is the engine's `HlcClock`; `BiTemporal::now(clock)` stamps
    /// the bi-temporal `(valid, sys)` pair at the moment of ingest.
    pub fn into_episode(self, scope: Scope, clock: &HlcClock) -> Episode {
        Episode {
            id: Ulid::new(),
            scope,
            source: self.source,
            content: self.content,
            t_ref: self.t_ref,
            bt: BiTemporal::now(clock),
            metadata: self.metadata,
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
