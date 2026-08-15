//! Ingest pipeline — INGEST-02..04.
//!
//! Wires the [`crate::chunker::chunk_markdown`] output through an [`Embedder`]
//! and into a SINGLE [`StoragePort::atomic_write`] call. The single-call
//! invariant is enforced by the design (one `Vec<WriteOp>` constructed,
//! one method invocation) and asserted by `tests/ingest_pipeline.rs::single_atomic_write_call`.
//!
//! ## Failure surfaces
//! - **chunker** never errors (returns `Vec<ChunkDraft>`)
//! - **embedder** errors per batch → fall back to per-chunk; per-chunk error
//!   surfaces immediately as `LunarisError::Storage(Backend(...))`
//! - **serde_json::to_vec** failure surfaces as `LunarisError::Storage(Serde(_))`
//! - **storage write** failure surfaces as
//!   `LunarisError::Storage(StorageError::*)` — caller sees no Episode or
//!   Chunks landed (Phase 1 atomicity contract).
//!
//! ## INGEST-04 single-call invariant
//!
//! The one executable write call lives in `assemble_and_write`.  All public
//! entry points — [`ingest_episode`], [`ingest_episode_with_counter`], and
//! [`ingest_episode_with_bakeoff`] — funnel through that helper.  No other
//! function in this file may call `storage.atomic_write`.

use std::sync::Arc;

use lunaris_core::{
    Chunk, Embedder, Episode, HlcClock, Lsn, LunarisError, StorageError, StoragePort, WriteOp,
    keyspace::{community_key, doctree_key},
};
use serde_json::json;

use crate::chunker::{
    BakoffConfig, ChunkDraft, TokenCounter, build_doctree, build_raptor_tree,
    chunk_markdown_with_headings, chunk_markdown_with_headings_with_counter,
    chunk_texts_for_community, run_bakeoff,
};
use crate::schema_gate::validate_chunk_metadata;
use crate::summarizer::{ExtractiveSummarizer, Summarizer as _, SummaryInput};
use crate::{chunk_key, episode_key};

/// Number of chunks per `embed_batch` call. Per blueprint §4.1 ingest hot path.
pub const INGEST_EMBED_BATCH_SIZE: usize = 32;

/// Pure resolver for the effective ingest-driver batch from a raw env value.
/// The embedder re-chunks to its own ceiling (`LUNARIS_EMBED_BATCH`), but the
/// driver must hand it at least that many chunks per call or the embedder can
/// never assemble a full large batch. We therefore feed
/// `max(INGEST_EMBED_BATCH_SIZE, requested)` — never below the blueprint
/// default, and design-for-failure on garbage/zero (falls back to the const).
fn resolve_ingest_batch_size(raw: Option<&str>) -> usize {
    let requested = raw
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(INGEST_EMBED_BATCH_SIZE);
    requested.max(INGEST_EMBED_BATCH_SIZE)
}

/// Effective ingest-driver batch, re-reading `LUNARIS_EMBED_BATCH` per call.
/// Deliberately uncached (issue #49): long-running daemons must observe env
/// updates, and one env read per embed batch is noise next to the embed call.
fn ingest_embed_batch_size() -> usize {
    resolve_ingest_batch_size(std::env::var("LUNARIS_EMBED_BATCH").ok().as_deref())
}

/// Hard cap (in bytes) on a community summary before it is stored AND embedded.
/// A summary is a retrieval handle for its subtree, not the subtree itself —
/// ~500 tokens is ample. The cap also closes an ingest-OOM at the source:
/// [`ExtractiveSummarizer`] falls back to the FULL child text when a chunk lacks
/// sentence terminals (conversation/JSON turns rarely have clean ones), so an
/// uncapped root-community summary concatenates whole sections and can reach the
/// embedder's 8192-token ceiling; batched, that padded the attention tensor to
/// tens/hundreds of GB and OOM-killed the haystack ingest.
/// the embedder's own internal batch planning is the (defense-in-depth)
/// guard — this keeps summaries short before they ever reach it.
const MAX_SUMMARY_BYTES: usize = 2048;

/// Truncate `s` to at most `max` bytes, snapping down to the nearest UTF-8 char
/// boundary so the result is always valid UTF-8.
fn truncate_summary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Default chunker target tokens per `chunk_markdown` invocation.
const DEFAULT_TARGET_TOKENS: usize = 500;
/// Default chunker overlap tokens.
const DEFAULT_OVERLAP_TOKENS: usize = 100;

/// Vector index name the chunk embeddings land in. Matches the
/// `chunks|entities|facts|communities` whitelist in the Phase 1
/// `MoonStorage::atomic_write`.
const CHUNK_VECTOR_INDEX: &str = "chunks";

/// Vector index name the community summary embeddings land in (Phase-30 B1).
const COMMUNITY_VECTOR_INDEX: &str = "communities";

/// Commit receipt for callers that need to follow up on the persisted episode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestReceipt {
    pub lsn: Lsn,
    pub episode_id: ulid::Ulid,
    pub chunk_ids: Vec<ulid::Ulid>,
}

#[derive(Clone, Debug)]
struct AssembleReceipt {
    lsn: Lsn,
    chunk_ids: Vec<ulid::Ulid>,
}

/// Run the full ingest pipeline for a single Episode.
///
/// Steps:
/// 1. `chunk_markdown(&episode.content, 500, 100)` → drafts.
/// 2. `embedder.embed_batch(&[..])` in batches of [`INGEST_EMBED_BATCH_SIZE`];
///    on batch error, fall back to per-chunk single-input calls (per INGEST-02).
/// 3. Construct typed [`Chunk`]s from drafts + embeddings.
/// 4. Build `Vec<WriteOp>`: one `KvPut` for the Episode + per-chunk
///    `KvPut` (chunk JSON) + `VectorUpsert` (chunk embedding + metadata).
/// 5. Issue a single atomic write via `assemble_and_write`. Return the [`Lsn`].
///
/// Returns `Lsn::ZERO`-ish patterns are never returned because Phase 1 backends
/// always issue a positive HLC at commit time; callers may still treat
/// `lsn.wall_ms == 0 && lsn.counter == 0` as "no write happened" defensively.
pub async fn ingest_episode<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
) -> Result<Lsn, LunarisError> {
    Ok(ingest_episode_with_receipt(storage, embedder, clock, episode).await?.lsn)
}

/// Run [`ingest_episode`] and return the committed episode/chunk identifiers.
pub async fn ingest_episode_with_receipt<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
) -> Result<IngestReceipt, LunarisError> {
    // Step 1: chunk + capture heading records for DocTree construction (STRUCT-02).
    // Uses the v0 surrogate counter for back-compatibility with existing callers.
    // Production code should prefer `ingest_episode_with_counter` with a real BPE
    // counter obtained via `make_token_counter(Some(tokenizer_path))`.
    let (drafts, heading_records) = chunk_markdown_with_headings(
        &episode.content,
        DEFAULT_TARGET_TOKENS,
        DEFAULT_OVERLAP_TOKENS,
    );
    ingest_episode_inner(storage, embedder, clock, episode, drafts, heading_records).await
}

/// Run the full ingest pipeline for a single Episode using a caller-supplied
/// [`TokenCounter`].
///
/// This is the canonical production path. When the calling layer has loaded a
/// BPE tokenizer (via [`crate::chunker::make_token_counter`]), it passes the
/// resulting `Arc<dyn TokenCounter + Send + Sync>` here so that chunking uses
/// real BPE token counts instead of the v0 `words×1.3` surrogate.
///
/// The surrogate fallback is preserved: callers that pass
/// `Arc::new(SurrogateTokenCounter)` get byte-identical behaviour to
/// [`ingest_episode`].
///
/// All invariants of [`ingest_episode`] hold unchanged:
/// - INGEST-04: exactly ONE write call (delegated to `assemble_and_write`).
/// - Embedding batch fallback (INGEST-02) is unchanged.
/// - The returned [`Lsn`] is the commit timestamp from the storage backend.
pub async fn ingest_episode_with_counter<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
    counter: Arc<dyn TokenCounter + Send + Sync>,
) -> Result<Lsn, LunarisError> {
    Ok(ingest_episode_with_counter_and_receipt(storage, embedder, clock, episode, counter)
        .await?
        .lsn)
}

/// Run [`ingest_episode_with_counter`] and return committed identifiers.
pub async fn ingest_episode_with_counter_and_receipt<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
    counter: Arc<dyn TokenCounter + Send + Sync>,
) -> Result<IngestReceipt, LunarisError> {
    // Step 1: chunk + capture heading records for DocTree construction (STRUCT-02).
    // Uses the caller-supplied counter — BPE when available, surrogate fallback.
    let (drafts, heading_records) = chunk_markdown_with_headings_with_counter(
        &episode.content,
        DEFAULT_TARGET_TOKENS,
        DEFAULT_OVERLAP_TOKENS,
        counter.as_ref(),
    );

    // Steps 2-5 are identical to ingest_episode; delegate to the shared helper.
    ingest_episode_inner(storage, embedder, clock, episode, drafts, heading_records).await
}

/// Shared pipeline body used by both [`ingest_episode`] and
/// [`ingest_episode_with_counter`] after chunking is complete.
///
/// Runs embedding → Chunk construction → delegates to [`assemble_and_write`]
/// for WriteOp assembly and the single atomic write (INGEST-04).
async fn ingest_episode_inner<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
    drafts: Vec<ChunkDraft>,
    heading_records: Vec<crate::chunker::HeadingRecord>,
) -> Result<IngestReceipt, LunarisError> {
    let episode_id = episode.id;
    // Step 2: embed in batches of 32 with per-chunk fallback
    let embeddings = embed_with_fallback(embedder, &drafts).await?;
    debug_assert_eq!(embeddings.len(), drafts.len());

    // Step 3: build typed Chunks (drafts + their freshly-issued embeddings)
    let mut chunks: Vec<Chunk> = Vec::with_capacity(drafts.len());
    for (draft, embedding) in drafts.into_iter().zip(embeddings.into_iter()) {
        let mut c = draft.into_chunk(episode.scope.clone(), episode.id, clock);
        c.embedding = Some(embedding);
        chunks.push(c);
    }

    // Steps 4+5: assemble WriteOps and issue the single atomic write (INGEST-04).
    let receipt =
        assemble_and_write(storage, embedder, &episode, chunks, &heading_records, clock).await?;
    Ok(IngestReceipt { lsn: receipt.lsn, episode_id, chunk_ids: receipt.chunk_ids })
}

/// Assemble the full `Vec<WriteOp>` for one Episode (DocTree + Episode KvPut +
/// per-chunk KvPut + VectorUpsert + community KvPut + community VectorUpsert)
/// and call `storage.atomic_write` exactly once.
///
/// # INGEST-04
///
/// This is the **only** place in `pipeline.rs` that calls `storage.atomic_write`.
/// Both the standard path ([`ingest_episode_inner`]) and the bake-off path
/// ([`ingest_episode_with_bakeoff`]) route through this function so the
/// single-write invariant holds regardless of which entry point is used.
///
/// # Clock threading
///
/// `clock` must be the same `HlcClock` instance used to stamp `Episode.bt` and
/// `Chunk.bt`. Threading it here ensures `Community.bt` is stamped by the same
/// clock — critical for `read_as_of(T)` correctness where all three primitive
/// types must be visible at the same logical time T.
///
/// # Embedder threading (Phase-30 B1)
///
/// `embedder` is the same instance already used to embed chunks. Community
/// summaries are embedded in one `embed_batch` call (with per-item fallback
/// mirroring INGEST-02) — single-pass: reuse the pipeline embedder, never
/// construct a new one.
async fn assemble_and_write<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    episode: &Episode,
    chunks: Vec<Chunk>,
    heading_records: &[crate::chunker::HeadingRecord],
    clock: &HlcClock,
) -> Result<AssembleReceipt, LunarisError> {
    let source_char_len = episode.content.chars().count();
    let doctree =
        build_doctree(heading_records, episode.scope.as_str(), &episode.source, source_char_len);
    let doctree_value = serde_json::to_vec(&doctree).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("doctree serialize: {e}")))
    })?;

    // Phase 29 TREE-01/STORE-01: build the RAPTOR community tree.
    // build_raptor_tree wires Chunk.parent_id; use wired_chunks for all
    // downstream serialisation so parent_id is persisted correctly.
    let (mut communities, wired_chunks) =
        build_raptor_tree(&doctree, &chunks, &episode.scope, &episode.source, clock);

    // Phase 29 TREE-02: summarise each community bottom-up (deepest level first).
    // ExtractiveSummarizer is the default: no LLM required, always succeeds.
    let summarizer = ExtractiveSummarizer::new();
    // Sort communities deepest-first so children are summarised before parents.
    let mut indexed: Vec<(usize, &lunaris_core::primitives::Community)> =
        communities.iter().enumerate().collect();
    indexed.sort_by(|a, b| b.1.level.cmp(&a.1.level));
    let ordered_indices: Vec<usize> = indexed.iter().map(|(i, _)| *i).collect();

    // Build SummaryInputs in bottom-up order.
    let inputs: Vec<SummaryInput> = ordered_indices
        .iter()
        .map(|&i| {
            let c = &communities[i];
            SummaryInput {
                node_id: c.id,
                child_texts: chunk_texts_for_community(c.id, &communities, &wired_chunks),
            }
        })
        .collect();
    let summaries = summarizer.summarize(&inputs).await;

    // Assign summaries back to communities (in the same bottom-up order),
    // capping each at MAX_SUMMARY_BYTES so neither storage nor the downstream
    // embed_batch ever sees a pathologically long "summary".
    for (order_pos, &community_idx) in ordered_indices.iter().enumerate() {
        communities[community_idx].summary =
            truncate_summary(&summaries[order_pos], MAX_SUMMARY_BYTES);
    }

    // Phase-30 B1: embed all community summaries (single-pass, reuse pipeline embedder).
    // One batch call for all summaries; on failure, degrade to per-item (INGEST-02 parity).
    // Bottom-up order is preserved via `ordered_indices`.
    {
        let summary_texts: Vec<&str> =
            ordered_indices.iter().map(|&i| communities[i].summary.as_str()).collect();

        let summary_embeddings: Vec<Vec<f32>> = match embedder.embed_batch(&summary_texts).await {
            Ok(rows) if rows.len() == summary_texts.len() => rows,
            Ok(rows) => {
                // Wrong-sized batch from the embedder — degrade to per-item.
                tracing::warn!(
                    expected = summary_texts.len(),
                    got = rows.len(),
                    "community summary embed_batch returned wrong row count; \
                     falling back to per-item"
                );
                embed_summaries_per_item(embedder, &summary_texts).await?
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    n = summary_texts.len(),
                    "community summary embed_batch failed; falling back to per-item"
                );
                embed_summaries_per_item(embedder, &summary_texts).await?
            }
        };

        for (order_pos, &community_idx) in ordered_indices.iter().enumerate() {
            communities[community_idx].summary_embedding =
                Some(summary_embeddings[order_pos].clone());
        }
    }

    // Invariant: every community must now carry a populated summary_embedding.
    debug_assert!(
        communities.iter().all(|c| c.summary_embedding.is_some()),
        "Phase-30 B1: every Community must have summary_embedding after embed"
    );

    // Assemble all WriteOps into the single batch.
    // Capacity: doctree + episode + 2×chunk + 2×community (KvPut + VectorUpsert).
    let mut ops: Vec<WriteOp> =
        Vec::with_capacity(2 + 2 * wired_chunks.len() + 2 * communities.len());
    let chunk_ids: Vec<ulid::Ulid> = wired_chunks.iter().map(|chunk| chunk.id).collect();

    ops.push(WriteOp::KvPut { key: doctree_key(&episode.scope, episode.id), value: doctree_value });

    let episode_value = serde_json::to_vec(episode).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("episode serialize: {e}")))
    })?;
    ops.push(WriteOp::KvPut { key: episode_key(&episode.scope, episode.id), value: episode_value });

    // Chunks: use wired_chunks so parent_id is serialised (TREE-01 wiring).
    for chunk in &wired_chunks {
        let chunk_value = serde_json::to_vec(chunk).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("chunk serialize: {e}")))
        })?;
        ops.push(WriteOp::KvPut { key: chunk_key(&episode.scope, chunk.id), value: chunk_value });
        let embedding =
            chunk.embedding.as_ref().expect("embedding assigned before assemble_and_write").clone();
        let metadata = json!({
            "episode_id": chunk.episode_id.to_string(),
            "heading_path": chunk.heading_path,
            "offset": chunk.offset,
            "text": chunk.text,
            "source": &episode.source,
        });
        validate_chunk_metadata(&metadata).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("schema gate: {e}")))
        })?;
        ops.push(WriteOp::VectorUpsert {
            index: CHUNK_VECTOR_INDEX.to_string(),
            id: chunk.id.to_bytes().to_vec(),
            embedding,
            metadata,
        });
    }

    // Communities: KvPut (KV hydration) + VectorUpsert (Phase-30 B1: communities index).
    // Both ops are added to the SAME single WriteOp vector — INGEST-04 is preserved.
    for community in &communities {
        // KvPut: persist the full Community struct (now with summary_embedding populated).
        let community_value = serde_json::to_vec(community).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("community serialize: {e}")))
        })?;
        ops.push(WriteOp::KvPut {
            key: community_key(&episode.scope, community.id),
            value: community_value,
        });

        // VectorUpsert: write summary embedding into the `communities` FT vector index.
        // `extract_content_for_index("communities", ..)` in atomic.rs reads
        // `metadata["summary"]` for BM25 content — the field MUST be present here.
        let embedding = community
            .summary_embedding
            .as_ref()
            .expect("summary_embedding assigned in Phase-30 B1 block above")
            .clone();
        let metadata = json!({
            "summary": community.summary,
            "level": community.level,
            "parent": community.parent.map(|p| p.to_string()),
        });
        ops.push(WriteOp::VectorUpsert {
            index: COMMUNITY_VECTOR_INDEX.to_string(),
            id: community.id.to_bytes().to_vec(),
            embedding,
            metadata,
        });

        // TODO(phase-future): push WriteOp::GraphEdge for parent-child edges (D7 deferred).
        // The tree is fully navigable via Community.parent / Community.members + Chunk.parent_id
        // without graph edges. Deferring avoids shipping an untested branch
        // (the graph-OFF path never exercises it — same trap as Phase-28 graph-ON RC).
    }

    // INGEST-04: the single atomic write for this episode (see module-level invariant).
    // NOTE: `maintenance_hint` below is a SEPARATE, best-effort call — it does
    // not count against the "exactly one atomic_write" invariant (INGEST-04
    // only constrains `atomic_write`, not maintenance signalling).
    let vector_upserts = ops.iter().filter(|op| matches!(op, WriteOp::VectorUpsert { .. })).count();
    let lsn = storage.atomic_write(&episode.scope, &ops).await?;

    // W3 (moon-v051-perf-exploit) — post-bulk-ingest maintenance hint.
    //
    // Cross-agent contract (FROZEN, see tmp/moon-perf-context.md): Agent A adds
    // `StoragePort::maintenance_hint` + `MaintenanceHint` to
    // `lunaris_core::storage::port`, defaulted to a no-op so every existing
    // backend keeps compiling without an override. This call is fully
    // best-effort: a failure here must NEVER fail an already-committed
    // ingest (the atomic_write above already succeeded), so errors are
    // logged and swallowed, not propagated.
    if let Err(e) = storage
        .maintenance_hint(
            &episode.scope,
            lunaris_core::storage::port::MaintenanceHint::BulkIngestComplete { vector_upserts },
        )
        .await
    {
        tracing::warn!(
            err = %e,
            vector_upserts,
            episode_id = %episode.id,
            "post-ingest maintenance_hint failed (best-effort, ingest already committed)"
        );
    }

    Ok(AssembleReceipt { lsn, chunk_ids })
}

/// Embed `drafts` in batches of [`INGEST_EMBED_BATCH_SIZE`]. On batch failure,
/// degrade to per-chunk single-input embeds (preserves the order). Per-chunk
/// failure surfaces to the caller (no further fallback — caller decides).
///
/// Returns one embedding per draft, in the same order as `drafts`.
async fn embed_with_fallback(
    embedder: &dyn Embedder,
    drafts: &[ChunkDraft],
) -> Result<Vec<Vec<f32>>, LunarisError> {
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(drafts.len());
    for batch in drafts.chunks(ingest_embed_batch_size()) {
        let texts: Vec<&str> = batch.iter().map(|d| d.text.as_str()).collect();
        match embedder.embed_batch(&texts).await {
            Ok(rows) => {
                if rows.len() != texts.len() {
                    // Defensive: an embedder that returns a wrong-sized batch
                    // is broken; degrade to per-chunk just like a hard error.
                    tracing::warn!(
                        expected = texts.len(),
                        got = rows.len(),
                        "embed_batch returned wrong row count; falling back to per-chunk"
                    );
                    for text in &texts {
                        let single = embedder.embed_batch(&[text]).await?;
                        match single.into_iter().next() {
                            Some(v) => out.push(v),
                            None => {
                                return Err(LunarisError::Storage(StorageError::Backend(
                                    "embed_batch returned 0 rows for single input".into(),
                                )));
                            }
                        }
                    }
                } else {
                    out.extend(rows);
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
                    match single.into_iter().next() {
                        Some(v) => out.push(v),
                        None => {
                            return Err(LunarisError::Storage(StorageError::Backend(
                                "embed_batch returned 0 rows for single input".into(),
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Fallback: embed community summary texts one-at-a-time when the batch call fails.
///
/// Mirrors the per-chunk fallback in [`embed_with_fallback`] (INGEST-02 parity).
/// Single-input per-item calls are tried individually; the first per-item error
/// surfaces to the caller immediately.
async fn embed_summaries_per_item(
    embedder: &dyn Embedder,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>, LunarisError> {
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for text in texts {
        let single = embedder.embed_batch(&[text]).await?;
        match single.into_iter().next() {
            Some(v) => out.push(v),
            None => {
                return Err(LunarisError::Storage(StorageError::Backend(
                    "community summary embed_batch returned 0 rows for single input".into(),
                )));
            }
        }
    }
    Ok(out)
}

/// Run the full ingest pipeline with the adaptive meta-framework bake-off.
///
/// When `bakeoff_config` is `Some`, this function:
/// 1. Runs [`run_bakeoff`] to select the best candidate chunk list.
/// 2. Uses the winner's **pre-computed embeddings** directly (SINGLE-PASS).
///    No additional `embed_batch` call is made for the winner's chunks.
/// 3. Passes the structural heading records (from the bake-off) to the DocTree.
/// 4. Delegates to `assemble_and_write` for WriteOp assembly and the single
///    atomic write (INGEST-04). Note: `assemble_and_write` makes one additional
///    `embed_batch` call for community summaries (Phase-30 B1) — this is not a
///    SINGLE-PASS violation because community summary texts are new, not re-embeddings
///    of the winner's chunk texts.
///
/// When `bakeoff_config` is `None`, falls back to [`ingest_episode_with_counter`]
/// (backward-compatible).
///
/// `target_tokens` and `overlap_tokens` govern the bake-off's internal generators;
/// they default to 500/100 when `bakeoff_config` is `None`.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_episode_with_bakeoff<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
    counter: std::sync::Arc<dyn TokenCounter + Send + Sync>,
    bakeoff_config: Option<std::sync::Arc<BakoffConfig>>,
    target_tokens: usize,
    overlap_tokens: usize,
) -> Result<Lsn, LunarisError> {
    let Some(config) = bakeoff_config else {
        // Fallback: standard counter-based ingest (backward-compatible).
        return ingest_episode_with_counter(storage, embedder, clock, episode, counter).await;
    };

    // Step 1: produce structural heading records for DocTree (always from structural parse,
    // independent of which candidate wins — heading structure is source-level metadata).
    let (_, heading_records) = chunk_markdown_with_headings_with_counter(
        &episode.content,
        target_tokens,
        overlap_tokens,
        counter.as_ref(),
    );

    // Step 2: run bake-off → winner drafts + winner embeddings (SINGLE-PASS).
    // run_bakeoff embeds unit texts once and each candidate's chunk texts once.
    // After this returns the winner's embeddings are in `winner.embeddings`.
    // The storage step MUST NOT call embed_batch again for the winner's chunks
    // (SINGLE-PASS invariant). Community summaries are a different text set and
    // are embedded inside assemble_and_write (Phase-30 B1).
    // Propagate Err: embedder failure in the bake-off path is a hard infra
    // failure (silent zero-fill would corrupt vector storage — see F1 fix).
    let winner = run_bakeoff(
        &episode.content,
        heading_records,
        &config,
        embedder,
        counter.as_ref(),
        target_tokens,
        overlap_tokens,
    )
    .await?;

    // Step 3: assemble Chunks from winner drafts + pre-computed embeddings.
    // SINGLE-PASS: no embed_batch call here — embeddings come from the bake-off.
    let mut chunks: Vec<Chunk> = Vec::with_capacity(winner.drafts.len());
    for (draft, embedding) in winner.drafts.into_iter().zip(winner.embeddings.into_iter()) {
        let mut c = draft.into_chunk(episode.scope.clone(), episode.id, clock);
        c.embedding = Some(embedding);
        chunks.push(c);
    }

    // Step 4: delegate to assemble_and_write — the single storage write call (INGEST-04).
    Ok(assemble_and_write(storage, embedder, &episode, chunks, &winner.heading_records, clock)
        .await?
        .lsn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_size_constant_is_32() {
        assert_eq!(INGEST_EMBED_BATCH_SIZE, 32);
    }

    #[test]
    fn ingest_batch_defaults_to_const_when_unset() {
        assert_eq!(resolve_ingest_batch_size(None), INGEST_EMBED_BATCH_SIZE);
    }

    #[test]
    fn ingest_batch_raises_for_large_override() {
        assert_eq!(resolve_ingest_batch_size(Some("128")), 128);
    }

    #[test]
    fn ingest_batch_never_drops_below_const() {
        // A small override must NOT shrink the driver below the blueprint floor.
        assert_eq!(resolve_ingest_batch_size(Some("4")), INGEST_EMBED_BATCH_SIZE);
    }

    #[test]
    fn ingest_batch_falls_back_on_garbage() {
        assert_eq!(resolve_ingest_batch_size(Some("0")), INGEST_EMBED_BATCH_SIZE);
        assert_eq!(resolve_ingest_batch_size(Some("xyz")), INGEST_EMBED_BATCH_SIZE);
    }

    #[test]
    fn ingest_batch_env_is_not_cached_forever() {
        // Issue #49: a once-init static froze the first LUNARIS_EMBED_BATCH read for
        // the process lifetime, so long-running daemons (contextd, mcp,
        // server) never observed config updates. The crate is
        // #![forbid(unsafe_code)], so a set_var-based behavioral test is not
        // writable here — pin the invariant structurally instead (the
        // eval_workflow_guard precedent): the env read must stay uncached.
        let src = include_str!("pipeline.rs");
        // Split needle so this test's own source never matches itself.
        let needle = concat!("Once", "Lock");
        assert!(
            !src.contains(needle),
            "ingest_embed_batch_size must re-read LUNARIS_EMBED_BATCH on \
             every call — no process-lifetime cache (issue #49)"
        );
    }

    #[test]
    fn truncate_summary_is_noop_under_cap() {
        let s = "short summary";
        assert_eq!(truncate_summary(s, MAX_SUMMARY_BYTES), s);
    }

    #[test]
    fn truncate_summary_caps_long_input() {
        let s = "x".repeat(MAX_SUMMARY_BYTES * 3);
        let out = truncate_summary(&s, MAX_SUMMARY_BYTES);
        assert_eq!(out.len(), MAX_SUMMARY_BYTES);
    }

    #[test]
    fn truncate_summary_snaps_to_char_boundary() {
        // 'é' is 2 bytes (U+00E9). A cap that lands mid-codepoint must snap DOWN
        // so the result is always valid UTF-8 (never panics on a byte split).
        let s = "é".repeat(100); // 200 bytes
        let out = truncate_summary(&s, 5); // 5 is mid-codepoint (odd byte)
        assert!(out.len() <= 5);
        assert_eq!(out.len() % 2, 0, "must snap to a 2-byte 'é' boundary");
        assert!(out.chars().all(|c| c == 'é'));
    }
}
