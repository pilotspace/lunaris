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
    keyspace::{fact_key as scoped_fact_key, fact_spo_key},
    sanitize_graph_ident,
};
use lunaris_extract::{ChunkInput, NeedsReviewItem, ValidatedExtraction, validate};
// B-4 verified at planning time (grep -nE on lunaris-ingest/src/lib.rs lines
// 18 + 25): chunk_markdown / chunk_key / episode_key / ChunkDraft are all
// publicly re-exported at the lunaris_ingest:: top level.
use lunaris_ingest::{
    BakoffConfig, ChunkDraft, HeadingRecord, TokenCounter, chunk_key, chunk_markdown_with_counter,
    chunk_markdown_with_headings_with_counter, episode_key, ingest_episode_with_bakeoff,
    run_bakeoff,
};
use serde_json::json;
// Plan 05-05 OPS-05 — `Instrument::instrument` wraps the per-call body in the
// `lunaris.ingest` info_span so per-call `correlation_id` field-recording
// + downstream child-span propagation works (CONTEXT.md D-24).
use tracing::Instrument;
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
/// Plan 04-04 D-16 — consolidate-queue topic. Every successful Lunaris::ingest
/// publishes one `__lunaris_consolidate__` event after the atomic_write
/// commits. Fire-and-forget; ingest still returns `Ok(Lsn)` on publish failure.
/// Closes CONSOL-05 (subscribe + publish wiring on the consolidator topic).
const CONSOLIDATE_QUEUE_TOPIC: &str = "__lunaris_consolidate__";
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
        // Plan 04-04 D-16: capture the episode_id BEFORE the move so we can
        // include it in the consolidate-queue envelope after the
        // atomic_write commits. The ingest functions consume `episode` so
        // we lift the id here.
        //
        // Plan 09.1-01 Task 2 (PRIM-04 full wiring): ALSO lift
        // `episode.source` before the move so the consolidate envelope can
        // carry it verbatim. Downstream `Consolidator::consolidate_scoped`
        // filters on `event.source.starts_with(prefix)`; without this the
        // scope filter degenerates to "match-none" for every non-empty
        // prefix (T-09-1-01-02 fail-closed posture).
        let episode_id = episode.id;
        let episode_source = episode.source.clone();
        // RFC 0001 Wave 1D: lift scope before move so publish_consolidate_event
        // and publish_needs_review can route queue messages under the correct
        // partition key instead of the Scope::dev() crutch.
        let episode_scope = episode.scope.clone();

        // Plan 05-05 OPS-05 — `lunaris.ingest` root span (CONTEXT.md D-24).
        // `correlation_id` is reserved as `tracing::field::Empty` so the
        // HTTP middleware (Plan 05-05 Task 3 `lunaris-server::middleware::tracing`)
        // can `Span::current().record("correlation_id", ...)` upstream of this
        // call site, OR an embedded caller can record it directly. Field
        // convention per CONTEXT.md `<specifics>` block: episode_id +
        // graph_enabled keep the span body greppable in JSON output without
        // leaking episode content (T-05-05-03 mitigation).
        let span = tracing::info_span!(
            "lunaris.ingest",
            correlation_id = tracing::field::Empty,
            episode_id = %episode_id,
            graph_enabled = self.graph_pipeline.is_enabled(),
        );
        // Snapshot the token_counter Arc before the async move so the closure
        // captures an owned Arc rather than a borrow of self (which moves into
        // the future). No lock involved — Arc::clone is cheap.
        let token_counter = self.token_counter.clone();
        async move {
            // Phase 28: snapshot bakeoff config before the async move so we
            // hold an owned Arc rather than a borrow of self.
            let bakeoff_config = self.bakeoff_config.clone();

            let lsn = if !self.graph_pipeline.is_enabled() {
                // Graph OFF — bakeoff path (Phase 28) or Phase 2 fast path.
                // INGEST-04: the single atomic_write call lives in
                // `assemble_and_write` inside lunaris_ingest::pipeline; both
                // ingest_episode_with_bakeoff and ingest_episode_with_counter
                // funnel through that helper.
                // Phase 28: if bakeoff_config is Some, use its target_tokens /
                // overlap_tokens so callers can tune chunk granularity via the
                // config rather than a separate parameter. Falls back to
                // DEFAULT_TARGET_TOKENS / DEFAULT_OVERLAP_TOKENS when None
                // (ingest_episode_with_bakeoff handles the None case internally
                // by delegating to ingest_episode_with_counter).
                let (target_tokens, overlap_tokens) = bakeoff_config
                    .as_deref()
                    .map(|c| (c.target_tokens, c.overlap_tokens))
                    .unwrap_or((DEFAULT_TARGET_TOKENS, DEFAULT_OVERLAP_TOKENS));
                ingest_episode_with_bakeoff(
                    self.storage.as_ref(),
                    self.embedder.as_ref(),
                    &self.clock,
                    episode,
                    token_counter.clone(),
                    bakeoff_config,
                    target_tokens,
                    overlap_tokens,
                )
                .await?
            } else {
                // Graph ON — extended fan-out with BPE token counter.
                // INGEST-04 single atomic_write call lives in
                // `ingest_episode_graph_on`.
                // Phase 28 T-28-07: thread bakeoff into graph-ON path.
                // bakeoff_config is None → existing path unchanged.
                ingest_episode_graph_on(
                    self.storage.as_ref(),
                    self.embedder.as_ref(),
                    &self.graph_pipeline,
                    &self.clock,
                    episode,
                    token_counter,
                    bakeoff_config,
                    graph_extract_per_session(),
                )
                .await?
            };

            // Plan 04-04 D-16: publish ONE __lunaris_consolidate__ event after
            // every successful atomic_write (both branches). Fire-and-forget —
            // a publish failure logs + continues; the ingest already committed
            // and returns Ok(Lsn). Closes CONSOL-05.
            //
            // Plan 09.1-01 Task 2 — `&episode_source` carries Episode.source
            // into the envelope so Consolidator::consolidate_scoped can
            // filter by `event.source.starts_with(prefix)` downstream.
            publish_consolidate_event(
                self.storage.as_ref(),
                episode_id,
                lsn,
                &episode_source,
                &episode_scope,
            )
            .await;

            Ok(lsn)
        }
        .instrument(span)
        .await
    }

    /// Phase 23 — agent-supplied structured ingest. See
    /// [`crate::structured_ingest`] for the design rationale and the
    /// EntityId-determinism guarantee.
    ///
    /// This entry point bypasses the LLM extractor entirely and writes the
    /// graph directly from the caller's `StructuredIngest` payload.
    /// **Always** writes the graph regardless of
    /// `LUNARIS_GRAPH_ENABLED` / `graph_pipeline().is_enabled()` — agents
    /// explicitly supplied entities, so the toggle does not gate them.
    /// (`is_enabled()` continues to gate ONLY the LLM-extractor branch in
    /// [`Self::ingest`].)
    ///
    /// `scope` is the partition key the resulting episode lives under;
    /// callers that already have a [`crate::ScopedLunaris`] should prefer
    /// `scoped.ingest_structured(payload)` which sources the scope from
    /// the wrapper.
    pub async fn ingest_structured(
        &self,
        payload: crate::structured_ingest::StructuredIngest,
        scope: lunaris_core::Scope,
    ) -> Result<Lsn, LunarisError> {
        crate::structured_ingest::ingest_structured_inner(
            self.storage.as_ref(),
            self.embedder.as_ref(),
            &self.clock,
            payload,
            scope,
        )
        .await
    }
}

/// Plan 04-04 D-16: publish one `__lunaris_consolidate__` envelope after
/// every successful Lunaris::ingest. The consolidator worker (Plan 04-02
/// `run_consolidate_worker`) debounces these per-episode_id (60 s default)
/// then flushes them through `Consolidator::consolidate`.
///
/// Envelope shape MUST match
/// [`lunaris_consolidate::types::ConsolidateEvent`] verbatim — the worker
/// deserializes via `serde_json::from_slice::<ConsolidateEvent>(&payload)`.
async fn publish_consolidate_event(
    storage: &dyn StoragePort,
    episode_id: Ulid,
    lsn: Lsn,
    source: &str,
    scope: &lunaris_core::Scope,
) {
    if !storage.capabilities().queue_native {
        tracing::debug!("consolidate queue unavailable; skipping consolidate-queue publish");
        return;
    }
    let envelope = json!({
        "kind": "ingest_committed",
        "episode_id": episode_id.to_string(),
        "lsn_wall_ms": lsn.wall_ms,
        "lsn_counter": lsn.counter,
        // Phase 9.1 Plan 01 Task 2 (PRIM-04 full wiring): Episode.source
        // flows into the envelope so Consolidator::consolidate_scoped's
        // default impl can filter batches by
        // `event.source.starts_with(scope_prefix)`. Paired with the
        // additive #[serde(default)] source: String field on
        // ConsolidateEvent so legacy payloads without this key still
        // deserialize as source: "".
        "source": source,
    });
    let payload = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                err = %e,
                "consolidate serialize failed; skipping consolidate-queue publish"
            );
            return;
        }
    };
    if let Err(e) = storage.publish(scope, CONSOLIDATE_QUEUE_TOPIC, 0, payload.into()).await {
        tracing::warn!(
            err = %e,
            "consolidate-queue publish failed; ingest still succeeded"
        );
    }
}

/// Resolve graph-extraction granularity from the
/// `LUNARIS_GRAPH_EXTRACT_GRANULARITY` env var. Read ONCE at the `ingest()`
/// boundary and threaded down as a param so the hot ingest function stays
/// env-free and unit-testable (mirrors `atomic.rs::snapshot_every_commit`).
fn graph_extract_per_session() -> bool {
    parse_graph_extract_granularity(
        std::env::var("LUNARIS_GRAPH_EXTRACT_GRANULARITY").ok().as_deref(),
    )
}

/// Pure parser for `LUNARIS_GRAPH_EXTRACT_GRANULARITY`. `None`/unset →
/// `false` (per-chunk, the pre-existing default). `session`/`episode`/`doc`
/// → `true` (one extraction call over the whole episode). `chunk`/`chunks`/
/// empty → `false`. Garbage → `false` with a `warn!` — this knob controls
/// remote token spend, so never silently flip it. Split out so it is
/// unit-testable without mutating the process environment (edition 2024:
/// `std::env::set_var` is `unsafe`, and this crate forbids it).
fn parse_graph_extract_granularity(raw: Option<&str>) -> bool {
    match raw {
        None => false,
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "session" | "episode" | "doc" => true,
            "chunk" | "chunks" | "" => false,
            other => {
                tracing::warn!(
                    value = %other,
                    "ignoring invalid LUNARIS_GRAPH_EXTRACT_GRANULARITY (want session|chunk); \
                     defaulting to chunk (per-chunk extraction)"
                );
                false
            }
        },
    }
}

/// Build the inputs handed to the graph [`Extractor`]. This is the
/// cost/quality lever for graph ingest:
///
/// * `per_session == false` (default) — one [`ChunkInput`] PER CHUNK. The
///   extractor makes one remote reasoning call per ~500-token chunk: maximal
///   locality, but hundreds of calls per multi-session document (which
///   saturates remote token-plan rate limits) and blind to relations that
///   span a chunk boundary.
/// * `per_session == true` — ONE [`ChunkInput`] over the whole episode text
///   (`episode.content`). ~`#chunks`× fewer remote calls AND captures
///   cross-turn relations within the session in a single pass. The chunk
///   vectors used for RETRIEVAL are untouched — only what the extractor sees
///   changes. The single input carries `episode_id` as its `chunk_id` (the
///   episode IS the unit) and an empty `heading_path`. Empty `content` yields
///   an empty vec so the extractor is skipped, matching the per-chunk path's
///   empty-chunks behaviour.
///
/// `reference_time_iso` (date-only, from [`Episode::t_ref`]) is stamped on
/// EVERY input — session-date grounding for the extraction prompt
/// (Mechanism B, 2026-07-29): an in-text date marker only survives into the
/// first chunk of a document, so threading the episode's real date here is
/// the only way later chunks get a `REFERENCE_TIME`.
fn build_extract_inputs(
    episode_id: Ulid,
    episode_content: &str,
    chunks: &[Chunk],
    per_session: bool,
    reference_time_iso: Option<&str>,
) -> Vec<ChunkInput> {
    let reference_time_iso = reference_time_iso.map(str::to_owned);
    if per_session {
        if episode_content.is_empty() {
            return Vec::new();
        }
        vec![ChunkInput {
            chunk_id: episode_id,
            text: episode_content.to_string(),
            heading_path: Vec::new(),
            reference_time_iso,
        }]
    } else {
        chunks
            .iter()
            .map(|c| ChunkInput {
                chunk_id: c.id,
                text: c.text.clone(),
                heading_path: c.heading_path.clone(),
                reference_time_iso: reference_time_iso.clone(),
            })
            .collect()
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
// Internal helper that threads the ingest dependencies (storage, embedder,
// graph handle, clock, episode, counter, bakeoff, granularity) — bundling
// them into a struct would only relocate the plumbing. `per_session_extract`
// (Phase-…: LUNARIS_GRAPH_EXTRACT_GRANULARITY) took it from 7 → 8 args.
#[allow(clippy::too_many_arguments)]
async fn ingest_episode_graph_on(
    storage: &dyn StoragePort,
    embedder: &dyn Embedder,
    graph_pipeline: &Arc<GraphPipelineHandle>,
    clock: &HlcClock,
    episode: Episode,
    counter: std::sync::Arc<dyn TokenCounter + Send + Sync>,
    bakeoff_config: Option<Arc<BakoffConfig>>,
    per_session_extract: bool,
) -> Result<Lsn, LunarisError> {
    // Step 1 + 2: chunk + embed.
    //
    // When bakeoff_config is Some: run the adaptive bake-off to select the
    // best chunking strategy. The winner's drafts + embeddings are reused
    // directly — no re-embed after selection (SINGLE-PASS invariant).
    //
    // When bakeoff_config is None: existing chunk_markdown_with_counter +
    // embed_with_fallback path, unchanged from pre-Phase-28 behavior.
    let chunks: Vec<Chunk> = if let Some(ref cfg) = bakeoff_config {
        let target_tokens = cfg.target_tokens;
        let overlap_tokens = cfg.overlap_tokens;
        // chunk_markdown_with_headings_with_counter returns (drafts, heading_records).
        // We need heading_records to pass into run_bakeoff's structural_heading_records param.
        let (_structural_drafts, heading_records): (Vec<ChunkDraft>, Vec<HeadingRecord>) =
            chunk_markdown_with_headings_with_counter(
                &episode.content,
                target_tokens,
                overlap_tokens,
                counter.as_ref(),
            );
        // run_bakeoff embeds unit texts once, scores all candidates, selects winner.
        // winner.embeddings are the chunk vectors from the scoring pass — reuse them.
        // SINGLE-PASS: do NOT call embed_with_fallback after this.
        // Propagate Err: embedder failure in the bake-off path is a hard infra
        // failure (silent zero-fill would corrupt vector storage — see F1 fix).
        let winner = run_bakeoff(
            &episode.content,
            heading_records,
            cfg,
            embedder,
            counter.as_ref(),
            target_tokens,
            overlap_tokens,
        )
        .await?;
        // TODO(phase-29): winner.heading_records is discarded here — the
        // graph-ON path does not persist a DocTree from the bake-off winner.
        // To fix, the graph-ON path needs its own `assemble_and_write`-style
        // helper that accepts heading_records and builds the DocTree WriteOp,
        // mirroring the graph-OFF path in `lunaris_ingest::pipeline::assemble_and_write`.
        let mut out: Vec<Chunk> = Vec::with_capacity(winner.drafts.len());
        for (draft, embedding) in winner.drafts.into_iter().zip(winner.embeddings.into_iter()) {
            let mut c = draft.into_chunk(episode.scope.clone(), episode.id, clock);
            c.embedding = Some(embedding);
            out.push(c);
        }
        out
    } else {
        // Existing path: chunk using the caller-supplied BPE counter.
        let drafts = chunk_markdown_with_counter(
            &episode.content,
            DEFAULT_TARGET_TOKENS,
            DEFAULT_OVERLAP_TOKENS,
            counter.as_ref(),
        );
        // Embed batch with per-chunk fallback.
        let embeddings = embed_with_fallback(embedder, &drafts).await?;
        debug_assert_eq!(embeddings.len(), drafts.len());
        let mut out: Vec<Chunk> = Vec::with_capacity(drafts.len());
        for (draft, embedding) in drafts.into_iter().zip(embeddings.into_iter()) {
            let mut c = draft.into_chunk(episode.scope.clone(), episode.id, clock);
            c.embedding = Some(embedding);
            out.push(c);
        }
        out
    };

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
    //
    // Plan 05-05 OPS-05 — `lunaris.extract` is a CHILD span of `lunaris.ingest`
    // (CONTEXT.md D-24). The B-NOTE under PATTERNS.md Plan 05-05 says
    // lunaris-extract has no `recall/run` entry to wrap directly; the span
    // wraps the Extractor call site here in `ingest_episode_graph_on`. Since
    // we're already inside the `lunaris.ingest` async block via the parent
    // `instrument(span)`, this nested info_span automatically attaches as a
    // child via tracing-subscriber's span-list emission.
    // `mut`: the Δ4 reconciliation pass below appends
    // `CrossEpisodeContradiction` items to `needs_review` before the
    // post-commit verify-queue publish.
    let mut validated: ValidatedExtraction = if extractor.applies() {
        // Granularity lever: per-chunk (default) vs per-session (one call over
        // the whole episode). See `build_extract_inputs`.
        //
        // Session-date grounding (Mechanism B, 2026-07-29): the episode's
        // real-world date reaches the extraction prompt as REFERENCE_TIME
        // (date-only — LME sessions and most provenance are date-grained).
        let reference_time_iso = episode.t_ref.map(|t| t.format("%Y-%m-%d").to_string());
        let chunk_inputs: Vec<ChunkInput> = build_extract_inputs(
            episode.id,
            &episode.content,
            &chunks,
            per_session_extract,
            reference_time_iso.as_deref(),
        );
        let extract_span = tracing::info_span!(
            "lunaris.extract",
            correlation_id = tracing::field::Empty,
            episode_id = %episode.id,
            chunk_count = chunk_inputs.len(),
        );
        let mut raw = extractor.extract(episode.id, &chunk_inputs).instrument(extract_span).await?;
        // Deterministic backstop for the observed model-"today" hallucination
        // class — runs AFTER extraction (and any extraction-cache replay), so
        // stale cached dates get capped too. See `cap_future_valid_from`.
        if let Some(ref_date) = reference_time_iso.as_deref() {
            lunaris_extract::cap_future_valid_from(&mut raw, ref_date);
        }
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
    // + 5 * facts.len() (KvPut + VectorUpsert{facts} + GraphNode + 2x
    //   GraphEdge — KG-RAG facts-as-graph-nodes, 2026-07-22)
    let cap = 1
        + 2 * chunks.len()
        + 2 * validated.entities.len()
        + validated.relations.len()
        + 5 * validated.facts.len();
    let mut ops: Vec<WriteOp> = Vec::with_capacity(cap);

    // Episode KvPut
    let episode_value = serde_json::to_vec(&episode).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("episode serialize: {e}")))
    })?;
    ops.push(WriteOp::KvPut { key: episode_key(&episode.scope, episode.id), value: episode_value });

    // Per-chunk KvPut + VectorUpsert (matches Phase 2 fast path verbatim).
    for chunk in &chunks {
        let chunk_value = serde_json::to_vec(chunk).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("chunk serialize: {e}")))
        })?;
        ops.push(WriteOp::KvPut { key: chunk_key(&episode.scope, chunk.id), value: chunk_value });
        let embedding = chunk.embedding.as_ref().expect("embedding assigned in step 2").clone();
        ops.push(WriteOp::VectorUpsert {
            index: CHUNK_VECTOR_INDEX.into(),
            id: chunk.id.to_bytes().to_vec(),
            embedding,
            // Gap 9 fix (2026-04-21): include `text` so both Postgres BM25
            // (`payload->>'text'` per migration 20260421_000004) AND Moon
            // BM25/HYBRID (per `extract_content_for_index`) can score the
            // chunk's content. Without this both backends silently miss
            // chunk recall.
            metadata: json!({
                "episode_id": chunk.episode_id.to_string(),
                "heading_path": chunk.heading_path,
                "offset": chunk.offset,
                "text": chunk.text,
                // Plan 09.1-02 Task 2b — Moon chunks FT index declares
                // `valid_time` NUMERIC via SchemaField::Numeric; the
                // atomic_write fan-out HSETs this field when present in
                // metadata. Postgres writes already carry `valid_from` via
                // the chunks table schema — this field closes the Moon side
                // of the parity contract so Filter::ValidTimeRange queries
                // match newly-ingested chunks.
                "valid_time_ms": chunk.bt.valid.0.wall_ms,
                // Plan 15-01 Task 2 — episode.source flows into chunk
                // metadata so atomic.rs can HSET it as a TAG field for
                // server-side `@source:{value}` FT.SEARCH (PERF-MOON-01).
                "source": &episode.source,
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
    // KG-RAG Wave C (2026-07-21): ONE batched real-embedding pass over
    // entity names + fact texts, replacing the det_vec hash stubs that made
    // the entities/facts HNSW legs geometrically random. Same fallback +
    // hard-error discipline as the chunk pass (embed_texts_with_fallback);
    // batching keeps this a single embedder round per ingest, so INGEST-04's
    // one-atomic-write invariant and the single-pass embedding rule both hold.
    let graph_texts: Vec<&str> = validated
        .entities
        .iter()
        .map(|e| e.name.as_str())
        .chain(validated.facts.iter().map(|f| f.fact_text.as_str()))
        .collect();
    let graph_vecs: Vec<Vec<f32>> = if graph_texts.is_empty() {
        Vec::new()
    } else {
        embed_texts_with_fallback(embedder, &graph_texts).await?
    };
    if graph_vecs.len() != graph_texts.len() {
        // embed_texts_with_fallback guarantees 1 row per input or Err; a
        // mismatch here would silently misalign fact vectors — fail loudly.
        return Err(LunarisError::Storage(StorageError::Backend(format!(
            "graph embedding row mismatch: {} texts, {} vectors",
            graph_texts.len(),
            graph_vecs.len()
        ))));
    }
    let (entity_vecs, fact_vecs) = graph_vecs.split_at(validated.entities.len());
    for (e, embedding) in validated.entities.iter().zip(entity_vecs) {
        // EntityId is `[u8; 16]`; flow it directly to the WriteOp id field.
        // (Ulid::from_bytes round-trip is lossless per the Plan 03-01
        // entity_id_to_ulid_bytes_are_lossless test.)
        let id_bytes = e.id.0.to_vec();
        ops.push(WriteOp::GraphNode {
            graph: GRAPH_NAME.into(),
            id: id_bytes.clone(),
            // T-01-03-01: free-form extractor output (MiniMax et al.) is not
            // grammar-constrained like Candle+GBNF, so entity_type can contain
            // spaces/punctuation that breaks Moon/AGE's Cypher parser when
            // interpolated raw as a node label. See sanitize_graph_ident doc.
            label: sanitize_graph_ident(&e.entity_type, "Entity"),
            props: json!({
                "id_hex": format!("{}", e.id),
                "name": e.name,
                "type": e.entity_type,
                "aliases": e.aliases,
                "confidence": e.confidence,
                "valid_from_iso": e.valid_from_iso,
                "valid_to_iso": e.valid_to_iso,
            }),
            index_kind: "entities".into(),
        });
        ops.push(WriteOp::VectorUpsert {
            index: ENTITIES_INDEX.into(),
            id: id_bytes,
            embedding: embedding.clone(),
            metadata: json!({"entity_type": e.entity_type, "name": e.name}),
        });
    }

    // NEW: per-relation GraphEdge.
    for r in &validated.relations {
        ops.push(WriteOp::GraphEdge {
            graph: GRAPH_NAME.into(),
            src: r.subject_id.0.to_vec(),
            dst: r.object_id.0.to_vec(),
            // T-01-03-01: same rationale as the GraphNode label above.
            rel: sanitize_graph_ident(&r.predicate, "RELATED_TO"),
            props: json!({
                "confidence": r.confidence,
                "valid_from_iso": r.valid_from_iso,
                "valid_to_iso": r.valid_to_iso,
            }),
        });
    }

    // NEW: per-fact KvPut + VectorUpsert{facts} + GraphNode + GraphEdge x2.
    //
    // KG-RAG facts-as-graph-nodes (2026-07-22): Facts join the entity graph
    // — `(subject)-[:HAS_FACT]->(fact:Fact)-[:FACT_ABOUT]->(object)` — so
    // FT.NAVIGATE hops from an `entities` KNN seed can reach real fact
    // CONTENT (dates, numbers, the actual sentence), not just entity names.
    // `index_kind: "facts"` makes atomic.rs register this node's `_key`
    // against the `facts` FT index (not `entities`) so `Navigate`'s
    // graph-expanded hits hydrate via the existing `fact_key` KV lookup.
    // Edge direction is arbitrary — Moon's graph-expand BFS traverses
    // `Direction::Both` (vendor/moon/src/command/vector_search/
    // graph_expand.rs) — chosen for readability if the raw graph is
    // inspected directly. `HAS_FACT`/`FACT_ABOUT` are fixed literals (not
    // extractor-controlled text), so no `sanitize_graph_ident` needed.
    //
    // Δ4 memory-update convergence (2026-07-30) — parity with
    // `structured_ingest`, which has carried this since the mem0-parity wave:
    //
    // * **Deterministic identity.** The row id is `FactId::from_triple`, NOT
    //   the extractor's `Ulid::new()`. Re-asserting the same
    //   (subject, predicate, object) in a later session overwrites in place
    //   instead of accruing a duplicate row. The validator's Wave-D dedup
    //   cannot cover this: it is within-batch only and keys on `valid_from`,
    //   which per-session date grounding makes distinct per session.
    // * **spo index + contradiction detection.** Each fact is classified
    //   against the in-scope `(subject, predicate)` rows; a different object
    //   with an OVERLAPPING window is a cross-episode contradiction, routed
    //   as a `NeedsReviewItem` to `__lunaris_verify__` for ASYNC arbitration
    //   (blueprint §3.2 subsystem 5 — arbitration is off the hot path, so
    //   `valid_to` is NOT stamped here; the verifier's `apply_supersede`
    //   closes the loser). Both facts stay stored meanwhile.
    // * **Fail-open.** A fact whose `valid_from` resolves from neither the
    //   extraction nor `episode.t_ref` is written additively and skipped by
    //   reconciliation entirely — an unknown window can never justify a
    //   supersede.
    //
    // The spo-index KvPuts join THIS `ops` vec; the reads happen before the
    // commit and do not count, so INGEST-04 (one `atomic_write`) holds.
    let spo_now = clock.tick();
    let episode_ref_date = episode.t_ref;
    let mut spo_index: std::collections::HashMap<Vec<u8>, Vec<crate::reconcile::SpoEntry>> =
        std::collections::HashMap::new();
    let mut spo_touched: Vec<Vec<u8>> = Vec::new();

    for (f, embedding) in validated.facts.iter().zip(fact_vecs) {
        // Deterministic identity replaces the extractor-supplied random id.
        let fact_id = Ulid::from_bytes(
            lunaris_extract::types::FactId::from_triple(f.subject_id, &f.predicate, f.object_id).0,
        );
        let mut fact_row = f.clone();
        fact_row.id = fact_id;

        // Reconcile against prior in-scope assertions of this
        // (subject, predicate), when the window is resolvable.
        if let Some(valid_from) = resolve_fact_instant(&f.valid_from_iso, episode_ref_date) {
            let valid_to = resolve_fact_instant(
                f.valid_to_iso.as_deref().unwrap_or_default(),
                // A missing valid_to means "still open" — never inherit the
                // episode date here or every fact would close immediately.
                None,
            );
            let spo_key = fact_spo_key(&episode.scope, &f.subject_id.0, &f.predicate);
            if !spo_index.contains_key(&spo_key) {
                let prior = crate::structured_ingest::read_spo_index(
                    storage,
                    &episode.scope,
                    &spo_key,
                    spo_now,
                )
                .await?;
                spo_index.insert(spo_key.clone(), prior);
                spo_touched.push(spo_key.clone());
            }
            let new_triple = crate::reconcile::FactTriple {
                subject_id: f.subject_id,
                predicate: f.predicate.clone(),
                object_id: f.object_id,
                valid_from,
                valid_to,
            };
            let prior = &spo_index[&spo_key];
            match crate::reconcile::classify_fact(&new_triple, prior) {
                crate::reconcile::FactDecision::Noop => {
                    // Same triple: the deterministic-id KvPut overwrites the
                    // row with the new window, so keep the index entry in sync
                    // or a later check classifies against a stale interval and
                    // can falsely supersede.
                    if let Some(entry) = spo_index
                        .get_mut(&spo_key)
                        .and_then(|v| v.iter_mut().find(|e| e.object_id == f.object_id))
                    {
                        entry.valid_from = valid_from;
                        entry.valid_to = valid_to;
                    }
                }
                crate::reconcile::FactDecision::Append => {
                    spo_index.get_mut(&spo_key).expect("seeded above").push(
                        crate::reconcile::SpoEntry {
                            object_id: f.object_id,
                            fact_id,
                            valid_from,
                            valid_to,
                        },
                    );
                }
                crate::reconcile::FactDecision::Supersede { loser_fact_id } => {
                    let existing_object = prior
                        .iter()
                        .find(|p| p.fact_id == loser_fact_id)
                        .map_or(f.object_id, |p| p.object_id);
                    validated.needs_review.push(NeedsReviewItem::Fact {
                        reason: lunaris_extract::NeedsReviewReason::CrossEpisodeContradiction {
                            subject: f.subject_id,
                            predicate: f.predicate.clone(),
                            existing_fact_id: loser_fact_id,
                            existing_object,
                            new_fact_id: fact_id,
                            new_object: f.object_id,
                        },
                        raw: fact_row.clone(),
                    });
                    spo_index.get_mut(&spo_key).expect("seeded above").push(
                        crate::reconcile::SpoEntry {
                            object_id: f.object_id,
                            fact_id,
                            valid_from,
                            valid_to,
                        },
                    );
                }
            }
        }

        let f = &fact_row;
        let fact_value = serde_json::to_vec(f).map_err(|err| {
            LunarisError::Storage(StorageError::Backend(format!("fact serialize: {err}")))
        })?;
        let fact_id_bytes = f.id.to_bytes().to_vec();
        ops.push(WriteOp::KvPut { key: scoped_fact_key(&episode.scope, f.id), value: fact_value });
        ops.push(WriteOp::VectorUpsert {
            index: FACTS_INDEX.into(),
            id: fact_id_bytes.clone(),
            embedding: embedding.clone(),
            metadata: json!({"predicate": f.predicate, "fact_text": f.fact_text}),
        });
        ops.push(WriteOp::GraphNode {
            graph: GRAPH_NAME.into(),
            id: fact_id_bytes.clone(),
            label: "Fact".into(),
            props: json!({
                "predicate": f.predicate,
                "confidence": f.confidence,
                "valid_from_iso": f.valid_from_iso,
                "valid_to_iso": f.valid_to_iso,
            }),
            index_kind: "facts".into(),
        });
        ops.push(WriteOp::GraphEdge {
            graph: GRAPH_NAME.into(),
            src: f.subject_id.0.to_vec(),
            dst: fact_id_bytes.clone(),
            rel: "HAS_FACT".into(),
            props: json!({}),
        });
        ops.push(WriteOp::GraphEdge {
            graph: GRAPH_NAME.into(),
            src: fact_id_bytes,
            dst: f.object_id.0.to_vec(),
            rel: "FACT_ABOUT".into(),
            props: json!({}),
        });
    }

    // Δ4: fold the updated spo-index rows into the SAME ops vec (INGEST-04).
    for key in spo_touched {
        let entries = &spo_index[&key];
        let value = serde_json::to_vec(&crate::structured_ingest::spo_entries_to_json(entries))
            .map_err(|err| {
                LunarisError::Storage(StorageError::Backend(format!("spo index serialize: {err}")))
            })?;
        ops.push(WriteOp::KvPut { key, value });
    }

    // Step 6: ONE atomic_write call (INGEST-04 + D-18 + T-03-03-06).
    // RFC 0001: pass episode scope as the partition key.
    let lsn = storage.atomic_write(&episode.scope, &ops).await?;

    // Step 7: NeedsReview items publish to verify queue (D-19 Phase 4 hook).
    // Non-blocking — failure logs + continues; the ingest still succeeded.
    // RFC 0001 Wave 1D: pass episode.scope so the verify queue publish carries
    // the correct partition key.
    publish_needs_review(storage, &episode.scope, &validated.needs_review).await;

    Ok(lsn)
}

/// Resolve an extraction timestamp string to an instant for reconciliation
/// windows, falling back to `fallback` (the episode's `t_ref`) when the string
/// is absent or unparseable.
///
/// Accepts BOTH shapes the extractor can emit: a full RFC3339 timestamp and
/// the bare `YYYY-MM-DD` date the session-date-grounded prompt asks for
/// (dates are taken at UTC midnight). Returns `None` when neither the string
/// nor the fallback yields an instant — the caller then skips reconciliation
/// for that fact rather than inventing a window (Δ4 fail-open).
fn resolve_fact_instant(
    iso: &str,
    fallback: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = iso.trim();
    if !s.is_empty() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&chrono::Utc));
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Some(chrono::DateTime::from_naive_utc_and_offset(
                d.and_time(chrono::NaiveTime::MIN),
                chrono::Utc,
            ));
        }
    }
    fallback
}

/// Mirror of [`lunaris_ingest::pipeline::embed_with_fallback`]. Reused inline
/// so the graph-ON branch can keep the chunk vector AND the embeddings AND
/// feed the Extractor in step 4 without three separate iteration passes.
async fn embed_with_fallback(
    embedder: &dyn Embedder,
    drafts: &[ChunkDraft],
) -> Result<Vec<Vec<f32>>, LunarisError> {
    let texts: Vec<&str> = drafts.iter().map(|d| d.text.as_str()).collect();
    embed_texts_with_fallback(embedder, &texts).await
}

/// Text-slice core of [`embed_with_fallback`] — KG-RAG Wave C reuses it for
/// the entity-name + fact-text pass in `ingest_episode_graph_on` so graph
/// primitives get REAL embeddings with the same batch-then-per-item fallback
/// discipline as chunks (and the same F1 rule: embedder failure is a hard
/// infra error, never a silent stub/zero fill).
async fn embed_texts_with_fallback(
    embedder: &dyn Embedder,
    all_texts: &[&str],
) -> Result<Vec<Vec<f32>>, LunarisError> {
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(all_texts.len());
    for batch in all_texts.chunks(EMBED_BATCH_SIZE) {
        let texts: Vec<&str> = batch.to_vec();
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
pub(crate) async fn publish_needs_review(
    storage: &dyn StoragePort,
    scope: &lunaris_core::Scope,
    items: &[NeedsReviewItem],
) {
    if !storage.capabilities().queue_native {
        tracing::debug!("verify queue unavailable; skipping verify-queue publish");
        return;
    }
    for item in items {
        let envelope = needs_review_envelope(item);
        let payload = match serde_json::to_vec(&envelope) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(err = %e, "needs_review serialize failed; skipping verify-queue publish");
                continue;
            }
        };
        if let Err(e) = storage.publish(scope, VERIFY_QUEUE_TOPIC, 0, payload.into()).await {
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

#[cfg(test)]
mod graph_granularity_tests {
    //! Per-session vs per-chunk graph-extraction granularity (the MiniMax
    //! token-plan cost lever). `build_extract_inputs` is a pure function so
    //! these prove the granularity WITHOUT a live model or mutating the
    //! process environment.
    use super::{build_extract_inputs, parse_graph_extract_granularity};
    use lunaris_core::{Chunk, HlcClock, Scope};
    use ulid::Ulid;

    fn chunk(text: &str) -> Chunk {
        let clock = HlcClock::new(1);
        Chunk::new(Scope::dev(), Ulid::new(), text, 10, 0, vec!["h".into()], &clock)
    }

    #[test]
    fn parse_granularity_maps_session_aliases_to_true_and_defaults_to_chunk() {
        // per-session aliases
        assert!(parse_graph_extract_granularity(Some("session")));
        assert!(parse_graph_extract_granularity(Some(" Episode ")));
        assert!(parse_graph_extract_granularity(Some("DOC")));
        // per-chunk + unset + empty + garbage all stay false (per-chunk)
        assert!(!parse_graph_extract_granularity(Some("chunk")));
        assert!(!parse_graph_extract_granularity(Some("chunks")));
        assert!(!parse_graph_extract_granularity(Some("")));
        assert!(!parse_graph_extract_granularity(None));
        assert!(!parse_graph_extract_granularity(Some("nonsense")));
    }

    #[test]
    fn per_session_yields_one_whole_session_input_not_per_chunk() {
        let eid = Ulid::new();
        let content = "user: hi\n\nassistant: hello\n\nuser: bye";
        // Even with THREE chunks present, per-session must collapse to ONE
        // input carrying the full episode content (keyed by episode id).
        let chunks = vec![chunk("user: hi"), chunk("assistant: hello"), chunk("user: bye")];
        let inputs = build_extract_inputs(eid, content, &chunks, true, Some("2023-05-30"));
        assert_eq!(inputs.len(), 1, "per-session must emit exactly one extractor input");
        assert_eq!(inputs[0].chunk_id, eid, "the single input is keyed by episode id");
        assert_eq!(inputs[0].text, content, "the single input carries the whole session text");
        assert!(inputs[0].heading_path.is_empty());
        assert_eq!(inputs[0].reference_time_iso.as_deref(), Some("2023-05-30"));
    }

    #[test]
    fn per_session_empty_content_skips_extraction() {
        let inputs = build_extract_inputs(Ulid::new(), "", &[], true, None);
        assert!(inputs.is_empty(), "empty episode content must skip the extractor");
    }

    #[test]
    fn per_chunk_yields_one_input_per_chunk_preserving_ids_and_text() {
        let chunks = vec![chunk("alpha"), chunk("beta"), chunk("gamma")];
        let inputs = build_extract_inputs(
            Ulid::new(),
            "alpha beta gamma",
            &chunks,
            false,
            Some("2023-05-30"),
        );
        assert_eq!(inputs.len(), 3, "per-chunk must emit one input per chunk");
        for (inp, ch) in inputs.iter().zip(chunks.iter()) {
            assert_eq!(inp.chunk_id, ch.id);
            assert_eq!(inp.text, ch.text);
            assert_eq!(inp.heading_path, ch.heading_path);
            assert_eq!(
                inp.reference_time_iso.as_deref(),
                Some("2023-05-30"),
                "EVERY per-chunk input carries the episode reference time"
            );
        }
    }
}
