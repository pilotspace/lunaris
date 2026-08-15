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

use std::collections::HashMap;

use lunaris_core::keyspace::{chunk_key, episode_key, fact_key as scoped_fact_key, fact_spo_key};
use lunaris_core::{
    Chunk, Embedder, Hlc, HlcClock, Lsn, LunarisError, Scope, StorageError, StoragePort, WriteOp,
    sanitize_graph_ident,
};
use lunaris_extract::types::{EntityId, Fact, FactId};
use lunaris_extract::validator::{NeedsReviewItem, NeedsReviewReason};
use lunaris_ingest::chunk_markdown;

use crate::episode_builder::EpisodeBuilder;
use crate::reconcile::{FactDecision, FactTriple, SpoEntry, classify_fact};

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

/// Internal implementation. The public surface is
/// [`crate::Lunaris::ingest_structured`] /
/// [`crate::ScopedLunaris::ingest_structured`] which inject the storage,
/// embedder, and clock from the handle.
///
/// INGEST-04 invariant preserved: exactly ONE `atomic_write` call covers
/// all writes (episode KV + per-chunk KV/Vector + per-entity
/// GraphNode/Vector + per-relation GraphEdge + per-fact KV/Vector).
///
/// Exposed as `#[doc(hidden)] pub` (not part of the stable surface) so the
/// `memory-update-intelligence` integration tests can drive the REAL
/// production write path against a recording `StoragePort` double — proving
/// the dedup + cross-episode-publish logic is actually WIRED into ingest, not
/// merely unit-correct in isolation. Production callers go through the handle.
#[doc(hidden)]
pub async fn ingest_structured_inner(
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
            // T-01-03-01: agent-supplied entity_type is untrusted free-form
            // text, same Cypher-injection/parse-break risk as extractor
            // output in crate::ingest. See sanitize_graph_ident doc.
            label: sanitize_graph_ident(&e.entity_type, "Entity"),
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
            index_kind: "entities".into(),
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
            // T-01-03-01: same rationale as the GraphNode label above.
            rel: sanitize_graph_ident(&r.predicate, "RELATED_TO"),
            props: json!({
                "confidence": r.confidence,
                "valid_from_iso": r.valid_from.to_rfc3339(),
                "valid_to_iso": r.valid_to.map(|t| t.to_rfc3339()),
                "source_episode_id": episode_id_str,
            }),
        });
    }

    // Per-fact KV + VectorUpsert, with memory-update convergence:
    //   - SYNC dedup: the fact id is the deterministic `FactId` of the
    //     (subject, predicate, object) triple, so re-asserting an identical
    //     fact overwrites the same row in place (no duplicate accrues).
    //   - CROSS-EPISODE contradiction detection: each fact is classified
    //     against the in-scope `(subject, predicate)` spo-index read once at
    //     `now`; an overlapping different-object assertion is collected as a
    //     `NeedsReviewItem` and published to the async verify queue AFTER the
    //     commit (the verifier closes the loser via `apply_supersede`).
    // The spo-index updates are folded into the SAME `ops` vec below, so the
    // single-`atomic_write` (INGEST-04) invariant holds. Reads do not count.
    let now_hlc = clock.tick();
    // spo-key → running entries (seeded from storage on first touch this call,
    // then mutated as additive/supersede facts are appended so multiple facts
    // sharing a (subject, predicate) in the SAME payload see each other).
    let mut spo_index: HashMap<Vec<u8>, Vec<SpoEntry>> = HashMap::new();
    let mut needs_review: Vec<NeedsReviewItem> = Vec::new();

    for (f, emb) in payload.facts.iter().zip(fact_embeds.iter()) {
        let sid = EntityId::from_name_and_type(&f.subject_name, &f.subject_type);
        let oid = EntityId::from_name_and_type(&f.object_name, &f.object_type);
        // Deterministic identity = sync dedup key.
        let fact_id = Ulid::from_bytes(FactId::from_triple(sid, &f.predicate, oid).0);

        // Seed the spo-index for this (subject, predicate) from storage once.
        let spo_key = fact_spo_key(&episode.scope, &sid.0, &f.predicate);
        if !spo_index.contains_key(&spo_key) {
            let prior = read_spo_index(storage, &episode.scope, &spo_key, now_hlc).await?;
            spo_index.insert(spo_key.clone(), prior);
        }

        let new_triple = FactTriple {
            subject_id: sid,
            predicate: f.predicate.clone(),
            object_id: oid,
            valid_from: f.valid_from,
            valid_to: f.valid_to,
        };
        let prior = &spo_index[&spo_key];
        match classify_fact(&new_triple, prior) {
            FactDecision::Noop => {
                // Exact re-assertion → dedup: the deterministic-id KvPut
                // overwrites the fact row IN PLACE with the new window, so keep
                // the matching spo-index entry's window in sync. Otherwise a
                // later cross-episode check classifies against a STALE interval
                // and can falsely supersede (BUG-1 / no_false_supersede).
                if let Some(entry) = spo_index
                    .get_mut(&spo_key)
                    .and_then(|v| v.iter_mut().find(|e| e.object_id == oid))
                {
                    entry.valid_from = f.valid_from;
                    entry.valid_to = f.valid_to;
                }
            }
            FactDecision::Append => {
                spo_index.get_mut(&spo_key).expect("seeded above").push(SpoEntry {
                    object_id: oid,
                    fact_id,
                    valid_from: f.valid_from,
                    valid_to: f.valid_to,
                });
            }
            FactDecision::Supersede { loser_fact_id } => {
                // The new fact is still written + indexed (additive); the
                // verifier closes the loser asynchronously.
                let existing_object =
                    prior.iter().find(|p| p.fact_id == loser_fact_id).map_or(oid, |p| p.object_id);
                needs_review.push(NeedsReviewItem::Fact {
                    reason: NeedsReviewReason::CrossEpisodeContradiction {
                        subject: sid,
                        predicate: f.predicate.clone(),
                        existing_fact_id: loser_fact_id,
                        existing_object,
                        new_fact_id: fact_id,
                        new_object: oid,
                    },
                    raw: Fact {
                        id: fact_id,
                        subject_id: sid,
                        predicate: f.predicate.clone(),
                        object_id: oid,
                        fact_text: f.fact_text.clone(),
                        confidence: f.confidence,
                        valid_from_iso: f.valid_from.to_rfc3339(),
                        valid_to_iso: f.valid_to.map(|t| t.to_rfc3339()),
                    },
                });
                spo_index.get_mut(&spo_key).expect("seeded above").push(SpoEntry {
                    object_id: oid,
                    fact_id,
                    valid_from: f.valid_from,
                    valid_to: f.valid_to,
                });
            }
        }

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

    // Fold the updated spo-index rows into the SAME atomic_write (one KvPut per
    // touched (subject, predicate) — INGEST-04 single-write preserved).
    for (key, entries) in &spo_index {
        let value = serde_json::to_vec(&spo_entries_to_json(entries)).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "structured_ingest: spo-index serialize: {e}"
            )))
        })?;
        ops.push(WriteOp::KvPut { key: key.clone(), value });
    }

    // ── 5. Single atomic_write (INGEST-04 invariant) ────────────────────
    let lsn = storage.atomic_write(&episode.scope, &ops).await?;

    // ── 6. Post-commit: publish cross-episode contradictions to the verify
    //       queue (side channel; the ingest already committed atomically).
    if !needs_review.is_empty() {
        crate::ingest::publish_needs_review(storage, &episode.scope, &needs_review).await;
    }

    Ok(lsn)
}

/// Read + parse the `(subject, predicate)` spo-index row at `as_of` into the
/// prior [`SpoEntry`] list consumed by [`classify_fact`]. A missing row (or an
/// empty/garbled value) yields an empty list — the first fact for a
/// `(subject, predicate)` is always additive.
async fn read_spo_index(
    storage: &dyn StoragePort,
    scope: &Scope,
    key: &[u8],
    as_of: Hlc,
) -> Result<Vec<SpoEntry>, LunarisError> {
    let Some(row) = storage.read_as_of(scope, key, as_of).await.map_err(LunarisError::Storage)?
    else {
        return Ok(Vec::new());
    };
    let arr: Vec<serde_json::Value> = serde_json::from_slice(&row.value).unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let (Some(obj_hex), Some(fid_str), Some(vf_str)) = (
            v.get("object_id").and_then(|x| x.as_str()),
            v.get("fact_id").and_then(|x| x.as_str()),
            v.get("valid_from").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        let (Some(object_id), Some(fact_id), Some(valid_from)) = (
            EntityId::from_hex(obj_hex),
            Ulid::from_string(fid_str).ok(),
            DateTime::parse_from_rfc3339(vf_str).ok().map(|d| d.with_timezone(&Utc)),
        ) else {
            continue;
        };
        let valid_to = v
            .get("valid_to")
            .and_then(|x| x.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        out.push(SpoEntry { object_id, fact_id, valid_from, valid_to });
    }
    Ok(out)
}

/// Serialize the spo-index entries to the canonical JSON array shape:
/// `[{object_id:hex, fact_id:ulid_str, valid_from:iso, valid_to:iso|null}]`.
fn spo_entries_to_json(entries: &[SpoEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .map(|e| {
            json!({
                "object_id": format!("{}", e.object_id),
                "fact_id": e.fact_id.to_string(),
                "valid_from": e.valid_from.to_rfc3339(),
                "valid_to": e.valid_to.map(|t| t.to_rfc3339()),
            })
        })
        .collect()
}
