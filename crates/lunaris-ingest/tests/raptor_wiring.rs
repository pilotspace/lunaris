//! Phase 29 Plan 03 — RAPTOR wiring discriminating test (Fixture 3).
//!
//! Proves that `assemble_and_write` is wired to call `build_raptor_tree` +
//! `ExtractiveSummarizer` — the "built != wired" discriminating test.

use std::collections::BTreeSet;

use futures::StreamExt as _;
use lunaris_core::{
    Episode, HlcClock, Scope, StubEmbedder,
    keyspace::{chunk_prefix, community_prefix},
    primitives::{Chunk, Community},
};
use lunaris_ingest::ingest_episode_with_raptor;
use lunaris_test_harness::open_test_storage;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Multi-section markdown document (H1 > H2 > H3) with paragraph text.
const HEADED_DOC: &str = "# Introduction

This section introduces the topic. It provides context.

## Background

Here is some background material. More details follow.

### Details

Detailed information about the subject matter. Even more text here.
";

fn headed_episode(clock: &HlcClock) -> Episode {
    Episode::new(Scope::dev(), "headed.md", HEADED_DOC, clock)
}

/// Helper: scan all keys under `prefix` and deserialize each value as `T`.
async fn scan_deserialize<T: serde::de::DeserializeOwned>(
    storage: &dyn lunaris_core::StoragePort,
    scope: &Scope,
    prefix: Vec<u8>,
) -> Vec<T> {
    let mut stream =
        storage.scan_range(scope, &prefix, None).await.expect("scan_range must succeed");
    let mut results = Vec::new();
    while let Some(item) = stream.next().await {
        let (_, value) = item.expect("scan_range item must not error");
        let t: T = serde_json::from_slice(&value).expect("value must deserialize");
        results.push(t);
    }
    results
}

// ---------------------------------------------------------------------------
// Fixture 3a: persisted H1-level Community.summary is non-empty
// ---------------------------------------------------------------------------

/// Proves the summarizer is actually invoked on ingest (built != wired).
/// H1 community has a non-empty summary that requires recursive leaf-text
/// aggregation (purely a members.len() > 0 check would pass even on a stub).
#[tokio::test]
async fn fixture_3a_h1_community_summary_non_empty() {
    // 0.7.0 port off `memory://` — harness-issued backend, degrading to
    // `memory://` where no Moon binary resolves. The binding owns the Moon
    // child, and `TestStorage` derefs to `Arc<dyn StoragePort>`.
    let storage = open_test_storage().await;
    let port = storage.port();
    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);
    let ep = headed_episode(&clock);
    let scope = ep.scope.clone();

    // W4.5: the RAPTOR community tree is OFF by default. Every assertion in this
    // file is a community-tree property, so it opts in explicitly rather than
    // relying on process env (unsafe under edition 2024, and racy in parallel).
    ingest_episode_with_raptor(&*port, &embedder, &clock, ep, true)
        .await
        .expect("ingest must succeed");

    let communities: Vec<Community> =
        scan_deserialize(&*port, &scope, community_prefix(&scope)).await;

    // Must have at least one Community with level == 1 (the H1 section).
    let h1_communities: Vec<&Community> = communities.iter().filter(|c| c.level == 1).collect();
    assert!(
        !h1_communities.is_empty(),
        "at least one level=1 Community must be persisted; got communities: {:?}",
        communities.iter().map(|c| (c.level, c.summary.len())).collect::<Vec<_>>()
    );

    // The discriminating check: H1 summary is non-empty.
    // This only passes if descendant leaf texts were recursively aggregated.
    for c in &h1_communities {
        assert!(
            !c.summary.is_empty(),
            "H1-level Community (id={}) must have a non-empty summary; \
             this proves the Summarizer was invoked (built != wired)",
            c.id
        );
    }

    // Phase-30 B1 populates `summary_embedding` in-memory at ingest and writes
    // it into the `communities` vector index (see
    // `community_vector_index_searchable_after_ingest` in ingest_pipeline.rs
    // for that proof). W3 (moon-v051-perf-exploit) then stopped serializing
    // the field into the KV `KvPut` payload — it duplicated ~80% of the
    // document's bytes for data the vector index already carries. A
    // `Community` round-tripped through `scan_deserialize` (KV JSON) below
    // therefore now sees `summary_embedding == None`; that is the new
    // contract, not a regression.
    for c in &communities {
        assert!(
            c.summary_embedding.is_none(),
            "Community (id={}, level={}) summary_embedding must be None after a KV round-trip \
             post-W3 (skip_serializing) — population is proven via the communities \
             VectorUpsert/vector_search path instead",
            c.id,
            c.level
        );
    }
}

// ---------------------------------------------------------------------------
// Fixture 3b: leaf chunk parent_id is set and points into the community set
// ---------------------------------------------------------------------------

/// Proves leaf chunks have non-None parent_id after ingest.
/// Uses a membership check (parent_id ∈ persisted community-ID set) — this
/// is robust even though Chunk IDs are random across runs.
#[tokio::test]
async fn fixture_3b_leaf_chunk_parent_id_set() {
    // 0.7.0 port off `memory://` — harness-issued backend, degrading to
    // `memory://` where no Moon binary resolves. The binding owns the Moon
    // child, and `TestStorage` derefs to `Arc<dyn StoragePort>`.
    let storage = open_test_storage().await;
    let port = storage.port();
    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);
    let ep = headed_episode(&clock);
    let scope = ep.scope.clone();

    // W4.5: the RAPTOR community tree is OFF by default. Every assertion in this
    // file is a community-tree property, so it opts in explicitly rather than
    // relying on process env (unsafe under edition 2024, and racy in parallel).
    ingest_episode_with_raptor(&*port, &embedder, &clock, ep, true)
        .await
        .expect("ingest must succeed");

    // Collect all persisted community IDs.
    let communities: Vec<Community> =
        scan_deserialize(&*port, &scope, community_prefix(&scope)).await;
    let community_ids: BTreeSet<ulid::Ulid> = communities.iter().map(|c| c.id).collect();
    assert!(!community_ids.is_empty(), "communities must be persisted");

    // Collect all persisted chunks.
    let chunks: Vec<Chunk> = scan_deserialize(&*port, &scope, chunk_prefix(&scope)).await;
    assert!(!chunks.is_empty(), "chunks must be persisted");

    // All chunks must have parent_id set and pointing into the community set.
    for chunk in &chunks {
        let pid = chunk.parent_id.expect(
            "every chunk in a headed document must have parent_id set after Phase 29 wiring",
        );
        assert!(
            community_ids.contains(&pid),
            "chunk (id={}) parent_id={} does not point to a known community",
            chunk.id,
            pid
        );
    }
}

// ---------------------------------------------------------------------------
// Summary-length cap: the ingest-OOM regression guard
// ---------------------------------------------------------------------------

/// Reproduces the haystack-ingest OOM at the data level: a long section with NO
/// sentence terminals makes `ExtractiveSummarizer::first_sentence` fall back to
/// the FULL child text, so the aggregated root-community summary would be
/// thousands of bytes. Uncapped, that reaches the embedder's 8192-token ceiling
/// and — batched across communities — padded a `[rows, heads, 8192, 8192]`
/// attention tensor to ~124 GB, OOM-killing ingest (and crashing Metal's buffer
/// pool). Every persisted community summary MUST now be ≤ the 2048-byte cap.
#[tokio::test]
async fn long_terminal_free_section_summary_is_capped() {
    // 0.7.0 port off `memory://` — harness-issued backend, degrading to
    // `memory://` where no Moon binary resolves. The binding owns the Moon
    // child, and `TestStorage` derefs to `Arc<dyn StoragePort>`.
    let storage = open_test_storage().await;
    let port = storage.port();
    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);

    // ~4200 bytes of words with NO '.', '?', or '!' → no sentence boundary, so
    // the extractive summarizer returns whole chunks and the section summary
    // balloons well past the 2048-byte cap absent the fix.
    let body = "alpha beta gamma delta epsilon zeta eta theta ".repeat(90);
    let content = format!("# Section\n\n{body}\n");
    let ep = Episode::new(Scope::dev(), "longsec.md", content, &clock);
    let scope = ep.scope.clone();

    // W4.5: the RAPTOR community tree is OFF by default. Every assertion in this
    // file is a community-tree property, so it opts in explicitly rather than
    // relying on process env (unsafe under edition 2024, and racy in parallel).
    ingest_episode_with_raptor(&*port, &embedder, &clock, ep, true)
        .await
        .expect("ingest must succeed");

    let communities: Vec<Community> =
        scan_deserialize(&*port, &scope, community_prefix(&scope)).await;
    assert!(!communities.is_empty(), "communities must be persisted");

    // The cap (the fix): no summary may exceed 2048 bytes, even when the
    // extractive no-terminal fallback returns whole sections.
    for c in &communities {
        assert!(
            c.summary.len() <= 2048,
            "community (id={}, level={}) summary is {} bytes; MAX_SUMMARY_BYTES (2048) cap not applied — \
             an uncapped summary OOM-kills the embedder",
            c.id,
            c.level,
            c.summary.len()
        );
    }

    // Discriminator: the fixture MUST actually exercise the long-summary path
    // (a trivially short fixture would pass the cap check vacuously). At least
    // one summary should be pushed up near the cap.
    assert!(
        communities.iter().any(|c| c.summary.len() > 1024),
        "fixture must produce at least one >1KB pre-cap summary to exercise the cap; \
         got lengths {:?}",
        communities.iter().map(|c| c.summary.len()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// INGEST-04 grep invariant: exactly ONE atomic_write call site in pipeline.rs
// ---------------------------------------------------------------------------

#[test]
fn raptor_wiring_ingest04_grep_stays_at_one() {
    // CARGO_MANIFEST_DIR points to crates/lunaris-ingest; pipeline.rs is under src/.
    let pipeline_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pipeline.rs");
    let src = std::fs::read_to_string(&pipeline_path)
        .unwrap_or_else(|e| panic!("pipeline.rs must be readable at {pipeline_path:?}: {e}"));
    let count = src.matches("storage.atomic_write(").count();
    assert_eq!(
        count, 1,
        "INGEST-04 violated: found {count} 'storage.atomic_write(' call sites in pipeline.rs \
         (must be exactly 1)"
    );
}
