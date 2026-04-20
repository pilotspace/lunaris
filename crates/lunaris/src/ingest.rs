//! `Lunaris::ingest` — Phase 3 dispatcher.
//!
//! When [`crate::graph_pipeline::GraphPipelineHandle::is_enabled`] returns
//! `false` (default per blueprint §5.2): delegates to
//! [`lunaris_ingest::ingest_episode`] (Phase 2 fast path) verbatim.
//! Byte-identical behavior to v0.0.1 — Phase 2's tests still pass.
//!
//! When the toggle is `true`: extends the Phase 2 pipeline with chunk-level
//! extraction + Validator-routing + per-extracted-Entity / Relation / Fact
//! `WriteOp` fan-out into the SAME [`lunaris_core::StoragePort::atomic_write`]
//! call (D-18 single-transaction contract; INGEST-04 single-call invariant
//! preserved). Validator-flagged `NeedsReview` items publish to the
//! `__lunaris_verify__` MQ topic via [`lunaris_core::StoragePort::publish`]
//! AFTER the atomic_write commits — Phase 4 Verifier worker consumes them
//! (D-19).

use std::sync::Arc;

use lunaris_core::{
    Chunk, Embedder, Episode, HlcClock, Lsn, LunarisError, StorageError, StoragePort, WriteOp,
};
use lunaris_extract::{ChunkInput, NeedsReviewItem, ValidatedExtraction, validate};
// B-4 verified at planning time (grep -nE on lunaris-ingest/src/lib.rs lines
// 18 + 25): chunk_markdown / chunk_key / episode_key / ChunkDraft are all
// publicly re-exported at the lunaris_ingest:: top level.
use lunaris_ingest::{ChunkDraft, chunk_key, chunk_markdown, episode_key};
use serde_json::json;
use ulid::Ulid;

use crate::graph_pipeline::GraphPipelineHandle;
use crate::handle::Lunaris;

/// Vector index name shared with Plan 03-02's Graph::anchored compose
/// example — entities live alongside chunks/facts in the standard
/// `chunks|entities|facts|communities` whitelist.
const ENTITIES_INDEX: &str = "entities";
/// Vector index name for facts (mirrors entities).
const FACTS_INDEX: &str = "facts";
/// Graph name shared with Plan 03-02's Graph::anchored Cypher template.
/// Matches `LUNARIS_GRAPH_NAME` in lunaris-retrieve.
const GRAPH_NAME: &str = "lunaris_graph";
/// Verify-queue topic the Phase 4 Verifier worker subscribes to (D-19 hook).
/// Phase 3 emits; Phase 4 consumes.
const VERIFY_QUEUE_TOPIC: &str = "__lunaris_verify__";
/// Vector index name for chunks (mirrors lunaris_ingest::pipeline).
const CHUNK_VECTOR_INDEX: &str = "chunks";
/// Default chunker target tokens (mirrors lunaris_ingest::pipeline).
const DEFAULT_TARGET_TOKENS: usize = 500;
/// Default chunker overlap tokens (mirrors lunaris_ingest::pipeline).
const DEFAULT_OVERLAP_TOKENS: usize = 100;
/// Embed-batch size (mirrors lunaris_ingest::pipeline INGEST_EMBED_BATCH_SIZE).
const EMBED_BATCH_SIZE: usize = 32;

impl Lunaris {
    /// Ingest one [`Episode`] through the appropriate pipeline based on
    /// `graph_pipeline().is_enabled()`.
    ///
    /// **Coherent per-call snapshot (T-03-03-01 mitigation):** the toggle bit
    /// is read ONCE at the top of this function. A toggle change during the
    /// in-flight ingest takes effect on the NEXT call, never mid-call. The
    /// `snapshot_extractor()` Arc is also captured ONCE in the graph-ON
    /// branch before any await.
    pub async fn ingest(&self, episode: Episode) -> Result<Lsn, LunarisError> {
        if !self.graph_pipeline.is_enabled() {
            // Graph OFF — Phase 2 fast path, verbatim. INGEST-04 single
            // atomic_write call lives inside `lunaris_ingest::ingest_episode`.
            return lunaris_ingest::ingest_episode(
                self.storage.as_ref(),
                self.embedder.as_ref(),
                &self.clock,
                episode,
            )
            .await;
        }
        // Graph ON — extended fan-out. INGEST-04 single atomic_write call
        // lives in `ingest_episode_graph_on` below.
        ingest_episode_graph_on(
            self.storage.as_ref(),
            self.embedder.as_ref(),
            &self.graph_pipeline,
            &self.clock,
            episode,
        )
        .await
    }
}

/// The graph-ON ingest path. Steps:
///
/// 1. Chunk markdown (reuse Plan 02-01 chunker via `lunaris_ingest::chunk_markdown`).
/// 2. Embed chunks in batches of 32 with per-chunk fallback (reuse pattern
///    from `lunaris_ingest::pipeline::embed_with_fallback`).
/// 3. Snapshot extractor out of the GraphPipelineHandle's RwLock — drops
///    guard BEFORE await per CLAUDE.md.
/// 4. Extract per-chunk → validator::validate → ValidatedExtraction.
///    NoopExtractor short-circuit: `applies()==false` → skip the extract
///    call entirely (T-03-03-05 — NoopExtractor cannot inject hidden
///    GraphNodes).
/// 5. Build SINGLE Vec<WriteOp>: Episode KvPut + per-chunk (KvPut +
///    VectorUpsert{chunks}) + per-extracted-entity (GraphNode +
///    VectorUpsert{entities}) + per-relation (GraphEdge) + per-fact
///    (KvPut + VectorUpsert{facts}). The cross-plan W-7 contract holds:
///    GraphNode props writes `id_hex` matching Plan 03-02's
///    `MATCH (n {id_hex: sid})` Cypher.
/// 6. ONE atomic_write call (INGEST-04 + D-18 single-transaction).
/// 7. After commit: publish one `__lunaris_verify__` message per
///    NeedsReview item (D-19 Phase 4 hook). Errors here are non-blocking —
///    `tracing::warn!` and continue. Ingest still returns `Ok(Lsn)`.
async fn ingest_episode_graph_on(
    storage: &dyn StoragePort,
    embedder: &dyn Embedder,
    graph_pipeline: &Arc<GraphPipelineHandle>,
    clock: &HlcClock,
    episode: Episode,
) -> Result<Lsn, LunarisError> {
    // Step 1: chunk
    let drafts = chunk_markdown(&episode.content, DEFAULT_TARGET_TOKENS, DEFAULT_OVERLAP_TOKENS);

    // Step 2: embed batch with per-chunk fallback
    let embeddings = embed_with_fallback(embedder, &drafts).await?;
    debug_assert_eq!(embeddings.len(), drafts.len());

    // Build typed Chunks
    let mut chunks: Vec<Chunk> = Vec::with_capacity(drafts.len());
    for (draft, embedding) in drafts.into_iter().zip(embeddings.into_iter()) {
        let mut c = draft.into_chunk(episode.id, clock);
        c.embedding = Some(embedding);
        chunks.push(c);
    }

    // Step 3: Snapshot extractor (CLAUDE.md "never hold lock across await")
    // T-03-03-01 — captured ONCE before any await; a mid-flight set_extractor
    // takes effect on the NEXT ingest call, never this one.
    let extractor = graph_pipeline.snapshot_extractor().ok_or_else(|| {
        LunarisError::Storage(StorageError::Backend(
            "graph_pipeline enabled but no extractor installed".into(),
        ))
    })?;

    // Step 4: Extract on each chunk (NoopExtractor short-circuit per
    // T-03-03-05 — applies()==false → empty ValidatedExtraction; no
    // GraphNode/Edge writes possible).
    let validated: ValidatedExtraction = if extractor.applies() {
        let chunk_inputs: Vec<ChunkInput> = chunks
            .iter()
            .map(|c| ChunkInput {
                chunk_id: c.id,
                text: c.text.clone(),
                heading_path: c.heading_path.clone(),
            })
            .collect();
        let raw = extractor.extract(episode.id, &chunk_inputs).await?;
        validate(raw)
    } else {
        // NoopExtractor — produces empty raw batch; validator returns empty.
        // Skip the extract call entirely — no model load, no async hop.
        ValidatedExtraction::default()
    };

    // Step 5: Build SINGLE Vec<WriteOp>. Capacity hint:
    //   1 (episode KvPut)
    // + 2 * chunks.len() (KvPut + VectorUpsert per chunk)
    // + 2 * entities.len() (GraphNode + VectorUpsert{entities})
    // + 1 * relations.len() (GraphEdge)
    // + 2 * facts.len() (KvPut + VectorUpsert{facts})
    let cap = 1
        + 2 * chunks.len()
        + 2 * validated.entities.len()
        + validated.relations.len()
        + 2 * validated.facts.len();
    let mut ops: Vec<WriteOp> = Vec::with_capacity(cap);

    // Episode KvPut
    let episode_value = serde_json::to_vec(&episode).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("episode serialize: {e}")))
    })?;
    ops.push(WriteOp::KvPut { key: episode_key(episode.id), value: episode_value });

    // Per-chunk KvPut + VectorUpsert (matches Phase 2 fast path verbatim).
    for chunk in &chunks {
        let chunk_value = serde_json::to_vec(chunk).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("chunk serialize: {e}")))
        })?;
        ops.push(WriteOp::KvPut { key: chunk_key(chunk.id), value: chunk_value });
        let embedding = chunk.embedding.as_ref().expect("embedding assigned in step 2").clone();
        ops.push(WriteOp::VectorUpsert {
            index: CHUNK_VECTOR_INDEX.into(),
            id: chunk.id.to_bytes().to_vec(),
            embedding,
            metadata: json!({
                "episode_id": chunk.episode_id.to_string(),
                "heading_path": chunk.heading_path,
                "offset": chunk.offset,
            }),
        });
    }

    // NEW: per-entity GraphNode + VectorUpsert{entities}.
    //
    // W-7 cross-plan contract: GraphNode props MUST contain `id_hex` so
    // Plan 03-02's Cypher `MATCH (n {id_hex: sid}) RETURN m.id_hex AS id`
    // round-trips. The `id_hex` value is the EntityId Display impl
    // (lowercase 32-char hex of the 16 byte content hash). Verified by the
    // `id_hex_round_trip_ingest_then_graph_anchored` smoke test in Task 3b.
    let embedder_dim = embedder.dim(); // W-2 verified: Embedder::dim() exists on the trait
    for e in &validated.entities {
        // EntityId is `[u8; 16]`; flow it directly to the WriteOp id field.
        // (Ulid::from_bytes round-trip is lossless per the Plan 03-01
        // entity_id_to_ulid_bytes_are_lossless test.)
        let id_bytes = e.id.0.to_vec();
        ops.push(WriteOp::GraphNode {
            graph: GRAPH_NAME.into(),
            id: id_bytes.clone(),
            label: e.entity_type.clone(),
            props: json!({
                "id_hex": format!("{}", e.id),
                "name": e.name,
                "type": e.entity_type,
                "aliases": e.aliases,
                "confidence": e.confidence,
                "valid_from_iso": e.valid_from_iso,
                "valid_to_iso": e.valid_to_iso,
            }),
        });
        // Stub embedding for entities (v0). Real entity embeddings land in
        // v1 alongside the extractor's output stage. The deterministic
        // hash-based vector keeps the vector index queryable for downstream
        // composers without burning a model forward pass per entity.
        let stub_embedding = det_vec(&e.name, embedder_dim);
        ops.push(WriteOp::VectorUpsert {
            index: ENTITIES_INDEX.into(),
            id: id_bytes,
            embedding: stub_embedding,
            metadata: json!({"entity_type": e.entity_type, "name": e.name}),
        });
    }

    // NEW: per-relation GraphEdge.
    for r in &validated.relations {
        ops.push(WriteOp::GraphEdge {
            graph: GRAPH_NAME.into(),
            src: r.subject_id.0.to_vec(),
            dst: r.object_id.0.to_vec(),
            rel: r.predicate.clone(),
            props: json!({
                "confidence": r.confidence,
                "valid_from_iso": r.valid_from_iso,
                "valid_to_iso": r.valid_to_iso,
            }),
        });
    }

    // NEW: per-fact KvPut + VectorUpsert{facts}.
    for f in &validated.facts {
        let fact_value = serde_json::to_vec(f).map_err(|err| {
            LunarisError::Storage(StorageError::Backend(format!("fact serialize: {err}")))
        })?;
        ops.push(WriteOp::KvPut { key: fact_key(f.id), value: fact_value });
        let stub_embedding = det_vec(&f.fact_text, embedder_dim);
        ops.push(WriteOp::VectorUpsert {
            index: FACTS_INDEX.into(),
            id: f.id.to_bytes().to_vec(),
            embedding: stub_embedding,
            metadata: json!({"predicate": f.predicate, "fact_text": f.fact_text}),
        });
    }

    // Step 6: ONE atomic_write call (INGEST-04 + D-18 + T-03-03-06).
    let lsn = storage.atomic_write(&ops).await?;

    // Step 7: NeedsReview items publish to verify queue (D-19 Phase 4 hook).
    // Non-blocking — failure logs + continues; the ingest still succeeded.
    publish_needs_review(storage, &validated.needs_review).await;

    Ok(lsn)
}

/// Mirror of [`lunaris_ingest::pipeline::embed_with_fallback`]. Reused inline
/// so the graph-ON branch can keep the chunk vector AND the embeddings AND
/// feed the Extractor in step 4 without three separate iteration passes.
async fn embed_with_fallback(
    embedder: &dyn Embedder,
    drafts: &[ChunkDraft],
) -> Result<Vec<Vec<f32>>, LunarisError> {
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(drafts.len());
    for batch in drafts.chunks(EMBED_BATCH_SIZE) {
        let texts: Vec<&str> = batch.iter().map(|d| d.text.as_str()).collect();
        match embedder.embed_batch(&texts).await {
            Ok(rows) if rows.len() == texts.len() => out.extend(rows),
            Ok(rows) => {
                tracing::warn!(
                    expected = texts.len(),
                    got = rows.len(),
                    "embed_batch returned wrong row count; falling back to per-chunk"
                );
                for text in &texts {
                    let single = embedder.embed_batch(&[text]).await?;
                    out.push(single.into_iter().next().ok_or_else(|| {
                        LunarisError::Storage(StorageError::Backend(
                            "embed_batch returned 0 rows for single input".into(),
                        ))
                    })?);
                }
            }
            Err(batch_err) => {
                tracing::warn!(
                    err = %batch_err,
                    batch_size = texts.len(),
                    "embed_batch failed; falling back to per-chunk"
                );
                for text in &texts {
                    let single = embedder.embed_batch(&[text]).await?;
                    out.push(single.into_iter().next().ok_or_else(|| {
                        LunarisError::Storage(StorageError::Backend(
                            "embed_batch returned 0 rows for single input".into(),
                        ))
                    })?);
                }
            }
        }
    }
    Ok(out)
}

/// Publish one `__lunaris_verify__` message per NeedsReview item. Errors are
/// non-blocking — log + continue. The ingest already committed atomically
/// before this fires (D-19 Phase 4 hook is a side channel).
///
/// ## Envelope shape
///
/// ```json
/// {
///   "kind": "entity" | "relation" | "fact",
///   "item": { "reason": <NeedsReviewReason>, "raw": <Entity|Relation|Fact> }
/// }
/// ```
///
/// The Phase 4 Verifier worker reads `kind` to pick the deserialize shape
/// for `item.raw`. Kept intentionally small and stable — adding fields here
/// is fine; renaming or removing requires a Phase 4 coordination ping.
async fn publish_needs_review(storage: &dyn StoragePort, items: &[NeedsReviewItem]) {
    for item in items {
        let envelope = needs_review_envelope(item);
        let payload = match serde_json::to_vec(&envelope) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(err = %e, "needs_review serialize failed; skipping verify-queue publish");
                continue;
            }
        };
        if let Err(e) = storage.publish(VERIFY_QUEUE_TOPIC, 0, payload.into()).await {
            tracing::warn!(err = %e, "verify-queue publish failed; ingest still succeeded");
        }
    }
}

/// Build the verify-queue envelope JSON for a single [`NeedsReviewItem`].
/// `NeedsReviewItem` is not directly `Serialize` (the variant carries a
/// `raw` field whose discriminator is the variant itself); we encode the
/// variant + reason + raw-as-json so the Phase 4 worker can route on `kind`.
fn needs_review_envelope(item: &NeedsReviewItem) -> serde_json::Value {
    match item {
        NeedsReviewItem::Entity { reason, raw } => json!({
            "kind": "entity",
            "item": { "reason": reason, "raw": raw },
        }),
        NeedsReviewItem::Relation { reason, raw } => json!({
            "kind": "relation",
            "item": { "reason": reason, "raw": raw },
        }),
        NeedsReviewItem::Fact { reason, raw } => json!({
            "kind": "fact",
            "item": { "reason": reason, "raw": raw },
        }),
    }
}

/// Deterministic stub embedding for entities + facts (v0). W-2 verified at
/// planning time: [`Embedder::dim`] EXISTS on the trait
/// (`lunaris-core/src/embedder.rs:14` — `fn dim(&self) -> usize`). This
/// helper is INLINED in lunaris/src/ingest.rs (NOT pulled from
/// lunaris-bench — that's a dev-dep and the dependency direction would
/// invert). v1 swaps to a real entity-embedding pass via the extractor.
fn det_vec(text: &str, dim: usize) -> Vec<f32> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    let mut state = h.finish().max(1);
    let mut v = Vec::with_capacity(dim);
    for _ in 0..dim {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bits = (state >> 33) as u32;
        v.push(((bits as f32) / (u32::MAX as f32)) - 0.5);
    }
    // L2-normalise so dot-product is bounded — matches the StubEmbedder
    // convention from lunaris-core for downstream rerank determinism.
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= norm;
    }
    v
}

/// Build the storage key for a Fact's KvPut. Mirrors lunaris_ingest's
/// `chunk_key` / `episode_key` prefix conventions: `fact:` ASCII prefix +
/// Ulid bytes. Distinct from chunk/episode keyspace so backend whitelists
/// don't collide.
fn fact_key(id: Ulid) -> Vec<u8> {
    let mut k = Vec::with_capacity(5 + 16);
    k.extend_from_slice(b"fact:");
    k.extend_from_slice(&id.to_bytes());
    k
}
