//! Phase 23 — agent-facing structured ingest.
//!
//! Lets an AI agent (or any caller that already knows the entity / relation
//! structure of a message) bypass the LLM extractor and write the graph
//! directly while still riding the same INGEST-04 single-`atomic_write`
//! invariant and the same deterministic `EntityId = blake3(name+type)[..16]`
//! dedup as the extractor-produced path.
//!
//! # Why this exists
//!
//! Many agents already produce structured `{entities, relations, facts}` as
//! a side-effect of their own reasoning. Round-tripping that knowledge as
//! prose through Lunaris's GBNF-constrained extractor pays two LLM passes
//! (extract here + verify downstream) and loses fidelity. This entry point
//! takes the structured payload directly.
//!
//! # Determinism = no lookup
//!
//! Because [`EntityId`] is the 16-byte truncation of
//! `blake3(normalize(canonical_name) || "::" || entity_type)`, an agent
//! that ingests
//!
//! ```text
//! RelationInput { subject_name: "Alice", subject_type: "Person",
//!                 predicate: "reports_to",
//!                 object_name: "Bob",   object_type: "Person", ... }
//! ```
//!
//! produces the **same** subject/object EntityIds whether the underlying
//! `Alice (Person)` node was created earlier by an LLM-extracted ingest,
//! a prior structured ingest, or this very call. The graph storage layer
//! dedups by key, so re-asserting an existing entity is a no-op and the
//! new edge attaches to the existing node — no GET-then-PUT round trip,
//! no race window.
//!
//! # Toggle gating
//!
//! Unlike the LLM extractor path, [`StructuredIngest`] **always** writes
//! the graph regardless of `LUNARIS_GRAPH_ENABLED` / `graph_pipeline()
//! .is_enabled()`. Rationale: the agent explicitly supplied entities —
//! they are not best-effort extraction. The pipeline toggle continues to
//! gate ONLY the LLM-extractor branch.
//!
//! # What this writes
//!
//! In one `atomic_write` per call:
//!
//! - Episode KV row (same shape as text ingest).
//! - Per-chunk KV + `VectorUpsert` (text chunked + embedded just like the
//!   text-ingest path; BM25 indexing piggybacks on the `content` metadata
//!   field).
//! - Per-entity `GraphNode` + `VectorUpsert{entities}`. The entity vector
//!   uses the caller's optional [`EntityInput::embedding`] when supplied;
//!   otherwise the handle's current `Embedder` embeds the entity name.
//! - Per-relation `GraphEdge` with `source_episode_id` stamped into the
//!   props.
//! - Per-fact KV + `VectorUpsert{facts}` (fact text embedded via the
//!   handle's `Embedder`).
//!
//! Provenance carried on edges/facts is **episode-level only** in v0.3:
//! `source_episode_id`. Per-chunk attribution (`source_chunk_id`) and
//! chunk-MENTIONS-entity edges land in a follow-up phase.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use ulid::Ulid;

use lunaris_core::keyspace::{chunk_key, episode_key, fact_key as scoped_fact_key};
use lunaris_core::{
    Chunk, Embedder, HlcClock, Lsn, LunarisError, Scope, StorageError, StoragePort, WriteOp,
};
use lunaris_extract::types::EntityId;
use lunaris_ingest::chunk_markdown;

use crate::episode_builder::EpisodeBuilder;

// Index names + graph name kept in sync with the LLM-extracted path in
// `crate::ingest`. Same string constants, kept private to this module so a
// refactor that moves them to a shared place can flip both call sites
// together.
const CHUNK_VECTOR_INDEX: &str = "chunks";
const ENTITIES_INDEX: &str = "entities";
const FACTS_INDEX: &str = "facts";
const GRAPH_NAME: &str = "lunaris_graph";

// Mirrors `lunaris_ingest::pipeline::DEFAULT_TARGET_TOKENS` /
// `DEFAULT_OVERLAP_TOKENS`. Inlined to avoid widening the
// `lunaris_ingest::pipeline` public surface; both numbers are stable across
// the chunker contract.
const DEFAULT_TARGET_TOKENS: usize = 256;
const DEFAULT_OVERLAP_TOKENS: usize = 32;

/// Default confidence for agent-supplied items. Agents that omit
/// confidence are taken at their word — they presumably know what they
/// asserted.
fn default_confidence() -> f32 {
    1.0
}

/// Agent-supplied entity. See module docs for the EntityId derivation
/// contract — the `(name, entity_type)` pair is the source of truth for
/// node identity; supplying a different alias for the same entity does
/// **not** create a new node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityInput {
    pub name: String,
    pub entity_type: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    pub valid_from: DateTime<Utc>,
    #[serde(default)]
    pub valid_to: Option<DateTime<Utc>>,
    /// Optional caller-supplied entity embedding. When `Some`, MUST match
    /// the handle's [`Embedder::dim`] — a mismatch surfaces as a
    /// `StorageError::Backend` at ingest time so the operator can correct
    /// the wheel build rather than silently corrupting the vector index.
    /// When `None`, Lunaris embeds [`Self::name`] via the handle's
    /// `Embedder`.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

/// Agent-supplied relation. Both endpoints are addressed by
/// `(name, entity_type)` — see module docs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelationInput {
    pub subject_name: String,
    pub subject_type: String,
    pub predicate: String,
    pub object_name: String,
    pub object_type: String,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    pub valid_from: DateTime<Utc>,
    #[serde(default)]
    pub valid_to: Option<DateTime<Utc>>,
}

/// Agent-supplied fact. `fact_text` is the natural-language rendering of
/// the `(subject, predicate, object)` triple; it is what gets embedded for
/// fact vector recall.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactInput {
    pub fact_text: String,
    pub subject_name: String,
    pub subject_type: String,
    pub predicate: String,
    pub object_name: String,
    pub object_type: String,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    pub valid_from: DateTime<Utc>,
    #[serde(default)]
    pub valid_to: Option<DateTime<Utc>>,
}

/// Top-level payload for [`crate::Lunaris::ingest_structured`] /
/// [`crate::ScopedLunaris::ingest_structured`].
///
/// `episode` carries the conversation-turn text (chunked + embedded like
/// the text-ingest path); the three vectors carry the agent's structured
/// knowledge. Any subset may be empty — an episode with only entities,
/// only relations, or no graph payload at all is valid (the latter
/// degenerates to a vanilla text ingest with the graph pipeline off).
pub struct StructuredIngest {
    pub episode: EpisodeBuilder,
    pub entities: Vec<EntityInput>,
    pub relations: Vec<RelationInput>,
    pub facts: Vec<FactInput>,
}

impl StructuredIngest {
    /// Construct a structured-ingest payload from an episode builder. All
    /// three structured lists start empty — chain `.with_entities(...)`
    /// etc. to populate.
    #[must_use]
    pub fn new(episode: EpisodeBuilder) -> Self {
        Self { episode, entities: Vec::new(), relations: Vec::new(), facts: Vec::new() }
    }

    #[must_use]
    pub fn with_entities(mut self, entities: Vec<EntityInput>) -> Self {
        self.entities = entities;
        self
    }

    #[must_use]
    pub fn with_relations(mut self, relations: Vec<RelationInput>) -> Self {
        self.relations = relations;
        self
    }

    #[must_use]
    pub fn with_facts(mut self, facts: Vec<FactInput>) -> Self {
        self.facts = facts;
        self
    }
}

/// Internal implementation. Crate-private; the public surface is
/// [`crate::Lunaris::ingest_structured`] /
/// [`crate::ScopedLunaris::ingest_structured`] which inject the storage,
/// embedder, and clock from the handle.
///
/// INGEST-04 invariant preserved: exactly ONE `atomic_write` call covers
/// all writes (episode KV + per-chunk KV/Vector + per-entity
/// GraphNode/Vector + per-relation GraphEdge + per-fact KV/Vector).
pub(crate) async fn ingest_structured_inner(
    storage: &dyn StoragePort,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    payload: StructuredIngest,
    scope: Scope,
) -> Result<Lsn, LunarisError> {
    let episode = payload.episode.into_episode(scope, clock);
    let embedder_dim = embedder.dim();

    // ── 1. Chunk + embed episode text ───────────────────────────────────
    let drafts = chunk_markdown(&episode.content, DEFAULT_TARGET_TOKENS, DEFAULT_OVERLAP_TOKENS);
    let chunk_embeddings: Vec<Vec<f32>> = if drafts.is_empty() {
        Vec::new()
    } else {
        let texts: Vec<&str> = drafts.iter().map(|d| d.text.as_str()).collect();
        let rows = embedder.embed_batch(&texts).await?;
        if rows.len() != texts.len() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "structured_ingest: chunk embed returned {} rows for {} chunks",
                rows.len(),
                texts.len()
            ))));
        }
        rows
    };

    // ── 2. Resolve entity embeddings ────────────────────────────────────
    // Caller-supplied entries are dim-validated up front; the rest go in
    // a single embed_batch call indexed by `to_embed_idx` so the order is
    // preserved when we splice results back.
    let mut entity_embeds: Vec<Vec<f32>> = vec![Vec::new(); payload.entities.len()];
    let mut to_embed_idx: Vec<usize> = Vec::new();
    let mut to_embed_text: Vec<String> = Vec::new();
    for (i, e) in payload.entities.iter().enumerate() {
        if let Some(emb) = &e.embedding {
            if emb.len() != embedder_dim {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "structured_ingest: EntityInput {:?} supplied embedding has dim {} but \
                     handle expects {}",
                    e.name,
                    emb.len(),
                    embedder_dim
                ))));
            }
            entity_embeds[i] = emb.clone();
        } else {
            to_embed_idx.push(i);
            to_embed_text.push(e.name.clone());
        }
    }
    if !to_embed_text.is_empty() {
        let texts: Vec<&str> = to_embed_text.iter().map(String::as_str).collect();
        let rows = embedder.embed_batch(&texts).await?;
        if rows.len() != to_embed_idx.len() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "structured_ingest: entity embed returned {} rows for {} entities",
                rows.len(),
                to_embed_idx.len()
            ))));
        }
        for (idx, emb) in to_embed_idx.into_iter().zip(rows.into_iter()) {
            entity_embeds[idx] = emb;
        }
    }

    // ── 3. Embed fact text in a single batch ────────────────────────────
    let fact_embeds: Vec<Vec<f32>> = if payload.facts.is_empty() {
        Vec::new()
    } else {
        let texts: Vec<&str> = payload.facts.iter().map(|f| f.fact_text.as_str()).collect();
        let rows = embedder.embed_batch(&texts).await?;
        if rows.len() != texts.len() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "structured_ingest: fact embed returned {} rows for {} facts",
                rows.len(),
                texts.len()
            ))));
        }
        rows
    };

    // ── 4. Assemble Vec<WriteOp> ────────────────────────────────────────
    let mut ops: Vec<WriteOp> = Vec::with_capacity(
        1 + 2 * drafts.len()
            + 2 * payload.entities.len()
            + payload.relations.len()
            + 2 * payload.facts.len(),
    );

    // Episode KV.
    let episode_value = serde_json::to_vec(&episode).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!(
            "structured_ingest: episode serialize: {e}"
        )))
    })?;
    ops.push(WriteOp::KvPut { key: episode_key(&episode.scope, episode.id), value: episode_value });

    // Per-chunk KV + Vector (BM25 piggybacks on `content` in metadata).
    let mut chunks: Vec<Chunk> = Vec::with_capacity(drafts.len());
    for (draft, emb) in drafts.into_iter().zip(chunk_embeddings.into_iter()) {
        let mut c = draft.into_chunk(episode.scope.clone(), episode.id, clock);
        c.embedding = Some(emb.clone());
        let chunk_value = serde_json::to_vec(&c).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "structured_ingest: chunk serialize: {e}"
            )))
        })?;
        ops.push(WriteOp::KvPut { key: chunk_key(&episode.scope, c.id), value: chunk_value });
        ops.push(WriteOp::VectorUpsert {
            index: CHUNK_VECTOR_INDEX.into(),
            id: c.id.to_bytes().to_vec(),
            embedding: emb,
            metadata: json!({
                "episode_id": c.episode_id.to_string(),
                "heading_path": c.heading_path,
                "offset": c.offset,
                "text": c.text,
                "source": &episode.source,
            }),
        });
        chunks.push(c);
    }

    // Per-entity GraphNode + VectorUpsert. EntityId is deterministic so
    // re-ingesting an existing logical entity collapses onto the existing
    // node at the storage-key layer.
    let episode_id_str = episode.id.to_string();
    for (e, emb) in payload.entities.iter().zip(entity_embeds.iter()) {
        let eid = EntityId::from_name_and_type(&e.name, &e.entity_type);
        let id_bytes = eid.0.to_vec();
        ops.push(WriteOp::GraphNode {
            graph: GRAPH_NAME.into(),
            id: id_bytes.clone(),
            label: e.entity_type.clone(),
            props: json!({
                "id_hex": format!("{eid}"),
                "name": e.name,
                "type": e.entity_type,
                "aliases": e.aliases,
                "confidence": e.confidence,
                "valid_from_iso": e.valid_from.to_rfc3339(),
                "valid_to_iso": e.valid_to.map(|t| t.to_rfc3339()),
                "source_episode_id": episode_id_str,
            }),
        });
        ops.push(WriteOp::VectorUpsert {
            index: ENTITIES_INDEX.into(),
            id: id_bytes,
            embedding: emb.clone(),
            metadata: json!({"entity_type": e.entity_type, "name": e.name}),
        });
    }

    // Per-relation GraphEdge with episode-level provenance.
    for r in &payload.relations {
        let sid = EntityId::from_name_and_type(&r.subject_name, &r.subject_type);
        let oid = EntityId::from_name_and_type(&r.object_name, &r.object_type);
        ops.push(WriteOp::GraphEdge {
            graph: GRAPH_NAME.into(),
            src: sid.0.to_vec(),
            dst: oid.0.to_vec(),
            rel: r.predicate.clone(),
            props: json!({
                "confidence": r.confidence,
                "valid_from_iso": r.valid_from.to_rfc3339(),
                "valid_to_iso": r.valid_to.map(|t| t.to_rfc3339()),
                "source_episode_id": episode_id_str,
            }),
        });
    }

    // Per-fact KV + VectorUpsert.
    for (f, emb) in payload.facts.iter().zip(fact_embeds.iter()) {
        let sid = EntityId::from_name_and_type(&f.subject_name, &f.subject_type);
        let oid = EntityId::from_name_and_type(&f.object_name, &f.object_type);
        let fact_id = Ulid::new();
        let fact_value = serde_json::to_vec(&serde_json::json!({
            "id": fact_id.to_string(),
            "subject_id": sid.0,
            "predicate": f.predicate,
            "object_id": oid.0,
            "fact_text": f.fact_text,
            "confidence": f.confidence,
            "valid_from_iso": f.valid_from.to_rfc3339(),
            "valid_to_iso": f.valid_to.map(|t| t.to_rfc3339()),
            "source_episode_id": episode_id_str,
        }))
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "structured_ingest: fact serialize: {e}"
            )))
        })?;
        ops.push(WriteOp::KvPut {
            key: scoped_fact_key(&episode.scope, fact_id),
            value: fact_value,
        });
        ops.push(WriteOp::VectorUpsert {
            index: FACTS_INDEX.into(),
            id: fact_id.to_bytes().to_vec(),
            embedding: emb.clone(),
            metadata: json!({"predicate": f.predicate, "fact_text": f.fact_text}),
        });
    }

    // ── 5. Single atomic_write (INGEST-04 invariant) ────────────────────
    let lsn = storage.atomic_write(&episode.scope, &ops).await?;
    Ok(lsn)
}
