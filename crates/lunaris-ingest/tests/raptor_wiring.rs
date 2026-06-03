//! Phase 29 Plan 03 — RAPTOR wiring discriminating test (Fixture 3).
//!
//! Proves that `assemble_and_write` is wired to call `build_raptor_tree` +
//! `ExtractiveSummarizer` — the "built != wired" discriminating test.

use std::collections::BTreeSet;

use futures::StreamExt as _;
use lunaris_core::{
    Episode, HlcClock, Scope, StoragePort, StubEmbedder,
    keyspace::{chunk_prefix, community_prefix},
    primitives::{Chunk, Community},
};
use lunaris_ingest::ingest_episode;
use lunaris_storage_embedded::EmbeddedStorage;

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
    storage: &EmbeddedStorage,
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
    let storage =
        EmbeddedStorage::connect("memory://").await.expect("embedded storage must connect");
    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);
    let ep = headed_episode(&clock);
    let scope = ep.scope.clone();

    ingest_episode(&storage, &embedder, &clock, ep).await.expect("ingest must succeed");

    let communities: Vec<Community> =
        scan_deserialize(&storage, &scope, community_prefix(&scope)).await;

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

    // Phase-30 B1: every persisted community must have summary_embedding populated.
    // (Replaces the Phase-29 D4 guard that asserted summary_embedding=None.)
    for c in &communities {
        let emb = c.summary_embedding.as_ref().unwrap_or_else(|| {
            panic!(
                "Community (id={}, level={}) must have summary_embedding=Some after Phase-30 B1",
                c.id, c.level
            )
        });
        assert_eq!(
            emb.len(),
            768,
            "Community (id={}, level={}) summary_embedding must be 768-d",
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
    let storage =
        EmbeddedStorage::connect("memory://").await.expect("embedded storage must connect");
    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);
    let ep = headed_episode(&clock);
    let scope = ep.scope.clone();

    ingest_episode(&storage, &embedder, &clock, ep).await.expect("ingest must succeed");

    // Collect all persisted community IDs.
    let communities: Vec<Community> =
        scan_deserialize(&storage, &scope, community_prefix(&scope)).await;
    let community_ids: BTreeSet<ulid::Ulid> = communities.iter().map(|c| c.id).collect();
    assert!(!community_ids.is_empty(), "communities must be persisted");

    // Collect all persisted chunks.
    let chunks: Vec<Chunk> = scan_deserialize(&storage, &scope, chunk_prefix(&scope)).await;
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
