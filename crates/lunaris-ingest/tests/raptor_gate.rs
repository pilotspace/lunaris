//! W4.5 — the RAPTOR community-tree write is gated OFF by default.
//!
//! ## Why the gate exists
//!
//! `assemble_and_write` builds a RAPTOR community tree, summarises it,
//! embeds every summary and writes `2 × N` extra `WriteOp`s into the
//! `communities` index on **every** ingest — including a heading-free
//! conversational turn, which still produces one synthetic root community
//! (`build_doctree` synthesises a root when `records.is_empty()`).
//!
//! Nothing on any production path reads it. `production_root`
//! (`lunaris-retrieve/src/composition.rs`) composes `chunks_leg` + `facts_leg`
//! only; the `communities` index is queried exclusively by the opt-in
//! `.tree(...)` DSL operator, which has no production caller.
//!
//! These tests pin BOTH directions:
//! - default (env unset) → zero community-tree work,
//! - flag ON → the tree is built exactly as before.
//!
//! They deliberately never mutate process env: edition 2024 makes
//! `std::env::set_var` `unsafe` and parallel tests race on it. The ON
//! direction goes through the explicit-flag entry point instead — the
//! `LUNARIS_RECALL_RERANK` precedent (`RecallRerankConfig::from_values`).

use async_trait::async_trait;
use futures::StreamExt as _;
use lunaris_core::{
    Embedder, Episode, HlcClock, LunarisError, Scope, StubEmbedder,
    keyspace::{chunk_prefix, community_prefix},
    primitives::{Chunk, Community},
};
use lunaris_ingest::ingest_episode;
use lunaris_test_harness::open_test_storage;
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Multi-section markdown (H1 > H2 > H3) — the RAPTOR-favourable shape.
const HEADED_DOC: &str = "# Introduction

This section introduces the topic. It provides context.

## Background

Here is some background material. More details follow.

### Details

Detailed information about the subject matter. Even more text here.
";

/// A heading-free conversational turn — the shape the MCP/hook path actually
/// ingests. `build_doctree` still synthesises ONE root community for it, so
/// this document is what makes the gate worth having.
const CHAT_TURN: &str = "The deploy went out at 14:02 and the error rate \
stayed flat. Tin asked for the rollback runbook to be linked from the \
release notes.";

/// Counts `embed_batch` invocations and every text handed to the embedder,
/// so a test can prove the community-summary embed call is not made at all.
struct CountingEmbedder {
    inner: StubEmbedder,
    calls: Mutex<usize>,
    texts: Mutex<Vec<String>>,
}

impl CountingEmbedder {
    fn new(dim: usize) -> Self {
        Self { inner: StubEmbedder::new(dim), calls: Mutex::new(0), texts: Mutex::new(Vec::new()) }
    }
    fn calls(&self) -> usize {
        *self.calls.lock()
    }
    fn texts(&self) -> Vec<String> {
        self.texts.lock().clone()
    }
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        *self.calls.lock() += 1;
        self.texts.lock().extend(inputs.iter().map(|s| s.to_string()));
        self.inner.embed_batch(inputs).await
    }
}

async fn scan_deserialize<T: serde::de::DeserializeOwned>(
    storage: &dyn lunaris_core::StoragePort,
    scope: &Scope,
    prefix: Vec<u8>,
) -> Vec<T> {
    let mut stream =
        storage.scan_range(scope, &prefix, None).await.expect("scan_range must succeed");
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let (_, value) = item.expect("scan_range item must not error");
        out.push(serde_json::from_slice(&value).expect("value must deserialize"));
    }
    out
}

// ---------------------------------------------------------------------------
// RED 1 — the default path writes NO community-tree artifacts
// ---------------------------------------------------------------------------

/// With `LUNARIS_RAPTOR_ENABLED` unset, an ingest of a *headed* document must
/// perform zero community-tree work:
/// - no `Community` KvPut lands under `community_prefix`,
/// - no chunk carries a `parent_id` (proves `build_raptor_tree` never ran),
/// - the embedder is called only for the chunk batches — never for a summary.
///
/// Chunks and the episode must still land: this gates RAPTOR, not ingest.
#[tokio::test]
async fn raptor_off_by_default_writes_no_community_artifacts() {
    let storage = open_test_storage().await;
    let port = storage.port();
    let embedder = CountingEmbedder::new(768);
    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::dev(), "headed.md", HEADED_DOC, &clock);
    let scope = ep.scope.clone();

    ingest_episode(&*port, &embedder, &clock, ep).await.expect("ingest must succeed");

    // (a) ingest still works — chunks landed.
    let chunks: Vec<Chunk> = scan_deserialize(&*port, &scope, chunk_prefix(&scope)).await;
    assert!(!chunks.is_empty(), "gating RAPTOR must not stop chunks being written");

    // (b) no community KV rows.
    let communities: Vec<Community> =
        scan_deserialize(&*port, &scope, community_prefix(&scope)).await;
    assert!(
        communities.is_empty(),
        "RAPTOR is OFF by default: expected 0 persisted communities, got {} \
         (levels: {:?}) — the tree is built but nothing on any production path \
         reads it, so the write must not happen unless LUNARIS_RAPTOR_ENABLED is set",
        communities.len(),
        communities.iter().map(|c| c.level).collect::<Vec<_>>()
    );

    // (c) no chunk was wired into a tree — proves build_raptor_tree did not run.
    let wired = chunks.iter().filter(|c| c.parent_id.is_some()).count();
    assert_eq!(
        wired,
        0,
        "RAPTOR OFF: no chunk may carry parent_id, got {}/{} wired",
        wired,
        chunks.len()
    );

    // (d) the embedder saw ONLY chunk texts — no summary embed call was made.
    // Exactly one `embed_batch` call is expected (all chunks fit the 32-wide
    // ingest batch); a second call is the community-summary embed.
    assert_eq!(
        embedder.calls(),
        1,
        "RAPTOR OFF: expected exactly 1 embed_batch call (the single chunk batch); \
         got {} — a second call is the community-summary embed that must not happen. \
         texts seen: {:?}",
        embedder.calls(),
        embedder.texts()
    );
}

/// The same guarantee for the shape the MCP/hook path actually ingests: a
/// heading-free turn. This is the expensive case — `build_doctree` synthesises
/// a root node, so RAPTOR-ON pays a full extra embedder round-trip and two
/// `WriteOp`s to produce ONE community that summarises the whole (single)
/// section back to itself.
#[tokio::test]
async fn raptor_off_by_default_skips_the_synthetic_root_on_a_heading_free_turn() {
    let storage = open_test_storage().await;
    let port = storage.port();
    let embedder = CountingEmbedder::new(768);
    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::dev(), "chat-turn", CHAT_TURN, &clock);
    let scope = ep.scope.clone();

    ingest_episode(&*port, &embedder, &clock, ep).await.expect("ingest must succeed");

    let communities: Vec<Community> =
        scan_deserialize(&*port, &scope, community_prefix(&scope)).await;
    assert!(
        communities.is_empty(),
        "a heading-free turn must produce no synthetic root community when \
         RAPTOR is OFF, got {}",
        communities.len()
    );
    assert_eq!(
        embedder.calls(),
        1,
        "a heading-free turn must cost exactly ONE embed_batch call, not two; \
         texts seen: {:?}",
        embedder.texts()
    );
}
