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
//! The one executable write call lives in [`assemble_and_write`].  All public
//! entry points — [`ingest_episode`], [`ingest_episode_with_counter`], and
//! [`ingest_episode_with_bakeoff`] — funnel through that helper.  No other
//! function in this file may call `storage.atomic_write`.

use std::sync::Arc;

use lunaris_core::{
    Chunk, Embedder, Episode, HlcClock, Lsn, LunarisError, StorageError, StoragePort, WriteOp,
    keyspace::doctree_key,
};
use serde_json::json;

use crate::chunker::{
    BakoffConfig, ChunkDraft, TokenCounter, build_doctree, chunk_markdown_with_headings,
    chunk_markdown_with_headings_with_counter, run_bakeoff,
};
use crate::schema_gate::validate_chunk_metadata;
use crate::{chunk_key, episode_key};

/// Number of chunks per `embed_batch` call. Per blueprint §4.1 ingest hot path.
pub const INGEST_EMBED_BATCH_SIZE: usize = 32;

/// Default chunker target tokens per `chunk_markdown` invocation.
const DEFAULT_TARGET_TOKENS: usize = 500;
/// Default chunker overlap tokens.
const DEFAULT_OVERLAP_TOKENS: usize = 100;

/// Vector index name the chunk embeddings land in. Matches the
/// `chunks|entities|facts|communities` whitelist in the Phase 1
/// `PostgresStorage::atomic_write` and `MoonStorage::atomic_write`.
const CHUNK_VECTOR_INDEX: &str = "chunks";

/// Run the full ingest pipeline for a single Episode.
///
/// Steps:
/// 1. `chunk_markdown(&episode.content, 500, 100)` → drafts.
/// 2. `embedder.embed_batch(&[..])` in batches of [`INGEST_EMBED_BATCH_SIZE`];
///    on batch error, fall back to per-chunk single-input calls (per INGEST-02).
/// 3. Construct typed [`Chunk`]s from drafts + embeddings.
/// 4. Build `Vec<WriteOp>`: one `KvPut` for the Episode + per-chunk
///    `KvPut` (chunk JSON) + `VectorUpsert` (chunk embedding + metadata).
/// 5. Issue a single atomic write via [`assemble_and_write`]. Return the [`Lsn`].
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
/// - INGEST-04: exactly ONE write call (delegated to [`assemble_and_write`]).
/// - Embedding batch fallback (INGEST-02) is unchanged.
/// - The returned [`Lsn`] is the commit timestamp from the storage backend.
pub async fn ingest_episode_with_counter<S: StoragePort + ?Sized>(
    storage: &S,
    embedder: &dyn Embedder,
    clock: &HlcClock,
    episode: Episode,
    counter: Arc<dyn TokenCounter + Send + Sync>,
) -> Result<Lsn, LunarisError> {
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
) -> Result<Lsn, LunarisError> {
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
    assemble_and_write(storage, &episode, chunks, &heading_records).await
}

/// Assemble the full `Vec<WriteOp>` for one Episode (DocTree + Episode KvPut +
/// per-chunk KvPut + VectorUpsert) and call `storage.atomic_write` exactly once.
///
/// # INGEST-04
///
/// This is the **only** place in `pipeline.rs` that calls `storage.atomic_write`.
/// Both the standard path ([`ingest_episode_inner`]) and the bake-off path
/// ([`ingest_episode_with_bakeoff`]) route through this function so the
/// single-write invariant holds regardless of which entry point is used.
async fn assemble_and_write<S: StoragePort + ?Sized>(
    storage: &S,
    episode: &Episode,
    chunks: Vec<Chunk>,
    heading_records: &[crate::chunker::HeadingRecord],
) -> Result<Lsn, LunarisError> {
    let source_char_len = episode.content.chars().count();
    let doctree =
        build_doctree(heading_records, episode.scope.as_str(), &episode.source, source_char_len);
    let doctree_value = serde_json::to_vec(&doctree).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("doctree serialize: {e}")))
    })?;

    let mut ops: Vec<WriteOp> = Vec::with_capacity(2 + 2 * chunks.len());
    ops.push(WriteOp::KvPut { key: doctree_key(&episode.scope, episode.id), value: doctree_value });

    let episode_value = serde_json::to_vec(episode).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("episode serialize: {e}")))
    })?;
    ops.push(WriteOp::KvPut { key: episode_key(&episode.scope, episode.id), value: episode_value });

    for chunk in &chunks {
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

    // INGEST-04: the single atomic write for this episode (see module-level invariant).
    let lsn = storage.atomic_write(&episode.scope, &ops).await?;
    Ok(lsn)
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
    for batch in drafts.chunks(INGEST_EMBED_BATCH_SIZE) {
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

/// Run the full ingest pipeline with the adaptive meta-framework bake-off.
///
/// When `bakeoff_config` is `Some`, this function:
/// 1. Runs [`run_bakeoff`] to select the best candidate chunk list.
/// 2. Uses the winner's **pre-computed embeddings** directly (SINGLE-PASS).
///    No additional `embed_batch` call is made for the winner.
/// 3. Passes the structural heading records (from the bake-off) to the DocTree.
/// 4. Delegates to [`assemble_and_write`] for WriteOp assembly and the single
///    atomic write (INGEST-04).
///
/// When `bakeoff_config` is `None`, falls back to [`ingest_episode_with_counter`]
/// (backward-compatible).
///
/// `target_tokens` and `overlap_tokens` govern the bake-off's internal generators;
/// they default to 500/100 when `bakeoff_config` is `None`.
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
    // The storage step MUST NOT call embed_batch again (SINGLE-PASS invariant).
    let winner = run_bakeoff(
        &episode.content,
        heading_records,
        &config,
        embedder,
        counter.as_ref(),
        target_tokens,
        overlap_tokens,
    )
    .await;

    // Step 3: assemble Chunks from winner drafts + pre-computed embeddings.
    // SINGLE-PASS: no embed_batch call here — embeddings come from the bake-off.
    let mut chunks: Vec<Chunk> = Vec::with_capacity(winner.drafts.len());
    for (draft, embedding) in winner.drafts.into_iter().zip(winner.embeddings.into_iter()) {
        let mut c = draft.into_chunk(episode.scope.clone(), episode.id, clock);
        c.embedding = Some(embedding);
        chunks.push(c);
    }

    // Step 4: delegate to assemble_and_write — the single storage write call (INGEST-04).
    assemble_and_write(storage, &episode, chunks, &winner.heading_records).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_size_constant_is_32() {
        assert_eq!(INGEST_EMBED_BATCH_SIZE, 32);
    }
}
