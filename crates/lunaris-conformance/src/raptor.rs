//! Phase 29 STORE-02/STORE-03 — RAPTOR idempotency and parity conformance suites.
//!
//! ## STORE-03 honesty note
//!
//! The SQLite self-parity arm (`fixture_5_parity_sqlite_self`) passes
//! unconditionally and demonstrates determinism at the code level.
//! Moon/SQLite cross-backend parity is **UAT-GATED** — requires `MOON_URL`
//! to be set. Not claimed green in autonomous CI runs.
//!
//! ## Members parity design
//!
//! `Community.members` contains both community IDs (deterministic blake3-derived)
//! and leaf chunk IDs (random `Ulid::new()`). For parity assertions:
//! - Community-typed members (IDs present in the community-ID set) are compared
//!   exactly — they are deterministic.
//! - Leaf members (IDs not in the community-ID set) are compared by COUNT only —
//!   chunk IDs differ between independent ingest runs.
//!
//! ## Idempotency semantics
//!
//! `scan_range(as_of=None)` returns the latest value per key (last-write-wins).
//! After re-ingest with deterministic community IDs, the scan returns the same
//! N keys with updated values — the BTreeSet of distinct IDs is unchanged.
//! This is the correct STORE-02 invariant: stable node-ID **set**, not row count.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::StreamExt as _;
use lunaris_core::{
    Episode, HlcClock, Scope, StoragePort, StubEmbedder, keyspace::community_prefix,
    primitives::Community,
};
use lunaris_ingest::ingest_episode;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Shared test fixture
// ---------------------------------------------------------------------------

/// 3-level heading document for all raptor conformance tests.
const RAPTOR_DOC: &str = "# Overview

This section provides an overview. Context is important here.

## Details

Here are the details. More information follows about the topic.

### Specifics

Specific information about the subject. Additional details provided.
";

/// Build a deterministic test episode using a fixed scope and source.
fn raptor_episode(clock: &HlcClock) -> Episode {
    Episode::new(Scope::dev(), "raptor_test.md", RAPTOR_DOC, clock)
}

// ---------------------------------------------------------------------------
// Helper: scan all Community values from storage
// ---------------------------------------------------------------------------

async fn scan_communities(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
) -> anyhow::Result<Vec<Community>> {
    let prefix = community_prefix(scope);
    let mut stream = storage.scan_range(scope, &prefix, None).await?;
    let mut communities = Vec::new();
    while let Some(item) = stream.next().await {
        let (_, value) = item.map_err(|e| anyhow::anyhow!("scan_range error: {e}"))?;
        let c: Community = serde_json::from_slice(&value)
            .map_err(|e| anyhow::anyhow!("community deserialize error: {e}"))?;
        communities.push(c);
    }
    Ok(communities)
}

// ---------------------------------------------------------------------------
// STORE-02: Idempotency suite
// ---------------------------------------------------------------------------

/// Run the RAPTOR idempotency suite against a single storage backend.
///
/// Ingests a 3-heading document twice and verifies that the Community
/// node-ID set (BTreeSet<Ulid>) is unchanged after re-ingest.
///
/// # NOTE on MVCC behaviour
///
/// MVCC may append historic rows; this test checks the current-time snapshot
/// node-ID set (BTreeSet of distinct Ulids), not raw row count. The scan_range
/// `as_of=None` returns the latest value per key, so duplicate ingests produce
/// the same key set.
pub async fn run_raptor_idempotency_suite(storage: Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let clock = HlcClock::new(0);
    let embedder = StubEmbedder::new(768);
    let scope = Scope::dev();

    // First ingest.
    let ep1 = raptor_episode(&clock);
    ingest_episode(&*storage, &embedder, &clock, ep1)
        .await
        .map_err(|e| anyhow::anyhow!("first ingest failed: {e}"))?;

    let communities_1 = scan_communities(&storage, &scope).await?;
    let ids_1: BTreeSet<Ulid> = communities_1.iter().map(|c| c.id).collect();

    anyhow::ensure!(!ids_1.is_empty(), "first ingest must produce at least one Community; got 0");

    // Second ingest (same document, same scope+source → deterministic IDs).
    let ep2 = raptor_episode(&clock);
    ingest_episode(&*storage, &embedder, &clock, ep2)
        .await
        .map_err(|e| anyhow::anyhow!("second ingest failed: {e}"))?;

    let communities_2 = scan_communities(&storage, &scope).await?;
    let ids_2: BTreeSet<Ulid> = communities_2.iter().map(|c| c.id).collect();

    // STORE-02: node-ID set must be unchanged after re-ingest.
    // NOTE: MVCC may append historic rows; this test checks the current-time
    // snapshot node-ID set (BTreeSet of distinct Ulids), not raw row count.
    anyhow::ensure!(
        ids_1 == ids_2,
        "STORE-02 idempotency violated: node-ID set changed after re-ingest.\n\
         Before: {:?}\nAfter: {:?}",
        ids_1,
        ids_2
    );

    anyhow::ensure!(
        ids_1.len() == 3,
        "expected 3 community nodes (H1, H2, H3) from the fixture document; got {}",
        ids_1.len()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// STORE-03: Parity suite
// ---------------------------------------------------------------------------

/// Run the RAPTOR parity suite against two independent storage backends.
///
/// Ingests the same document on each independently and asserts field equality:
/// - `id`, `level`, `parent` — exact equality (all deterministic).
/// - `members` — community-typed members exact equality; leaf member count equality.
/// - `summary` — non-empty on both.
/// - `summary_embedding` — `None` on both after the KV read-back (W3
///   embedding double-store fix: the field is `skip_serializing`, the vector
///   lives only in the `communities` vector index / vector column; Phase-30
///   B1 population-at-ingest is proven index-side by
///   `lunaris-ingest/tests/raptor_wiring.rs`, not through the KV blob).
///
/// Used for both SQLite self-parity (unconditional) and Moon vs SQLite (UAT-gated).
pub async fn run_raptor_parity_suite(
    storage_a: Arc<dyn StoragePort>,
    storage_b: Arc<dyn StoragePort>,
) -> anyhow::Result<()> {
    let clock = HlcClock::new(0);
    let embedder = StubEmbedder::new(768);
    let scope = Scope::dev();

    // Ingest independently on each backend.
    let ep_a = raptor_episode(&clock);
    ingest_episode(&*storage_a, &embedder, &clock, ep_a)
        .await
        .map_err(|e| anyhow::anyhow!("storage_a ingest failed: {e}"))?;

    let ep_b = raptor_episode(&clock);
    ingest_episode(&*storage_b, &embedder, &clock, ep_b)
        .await
        .map_err(|e| anyhow::anyhow!("storage_b ingest failed: {e}"))?;

    let mut communities_a = scan_communities(&storage_a, &scope).await?;
    let mut communities_b = scan_communities(&storage_b, &scope).await?;

    // Sort by community.id (Ulid bytes) for deterministic comparison order.
    communities_a.sort_by_key(|c| c.id.to_bytes());
    communities_b.sort_by_key(|c| c.id.to_bytes());

    anyhow::ensure!(
        communities_a.len() == communities_b.len(),
        "parity: community count differs: a={} b={}",
        communities_a.len(),
        communities_b.len()
    );

    // Build the community-ID sets for both backends (for member filtering).
    let community_ids_a: BTreeSet<Ulid> = communities_a.iter().map(|c| c.id).collect();
    let community_ids_b: BTreeSet<Ulid> = communities_b.iter().map(|c| c.id).collect();

    // The community-ID sets themselves must be equal.
    anyhow::ensure!(
        community_ids_a == community_ids_b,
        "parity: community-ID sets differ\na: {:?}\nb: {:?}",
        community_ids_a,
        community_ids_b
    );

    for (a, b) in communities_a.iter().zip(communities_b.iter()) {
        // id, level, parent: exact equality (deterministic).
        anyhow::ensure!(a.id == b.id, "parity: id mismatch: {:?} vs {:?}", a.id, b.id);
        anyhow::ensure!(
            a.level == b.level,
            "parity: level mismatch for community {}: {} vs {}",
            a.id,
            a.level,
            b.level
        );
        anyhow::ensure!(
            a.parent == b.parent,
            "parity: parent mismatch for community {}: {:?} vs {:?}",
            a.id,
            a.parent,
            b.parent
        );

        // Members: split into community-typed (deterministic) and leaf (random).
        let (comm_members_a, leaf_count_a) = split_members(&a.members, &community_ids_a);
        let (comm_members_b, leaf_count_b) = split_members(&b.members, &community_ids_b);

        // Community-typed members: exact equality.
        anyhow::ensure!(
            comm_members_a == comm_members_b,
            "parity: community-typed members differ for community {}:\na: {:?}\nb: {:?}",
            a.id,
            comm_members_a,
            comm_members_b
        );

        // Leaf members: count equality (chunk IDs are random across instances).
        anyhow::ensure!(
            leaf_count_a == leaf_count_b,
            "parity: leaf member count differs for community {}: a={} b={}",
            a.id,
            leaf_count_a,
            leaf_count_b
        );

        // Summary: non-empty on both.
        anyhow::ensure!(
            !a.summary.is_empty(),
            "parity: community {} summary is empty on storage_a",
            a.id
        );
        anyhow::ensure!(
            !b.summary.is_empty(),
            "parity: community {} summary is empty on storage_b",
            b.id
        );

        // W3 embedding double-store fix (moon-v051-perf-exploit): the KV
        // payload never carries the embedding (`skip_serializing`), so a
        // KV read-back MUST yield None on both backends. This replaces the
        // Phase-30 B1 `Some(768-d)` assertion, which probed population
        // through the (now intentionally slimmed) KV blob; population at
        // ingest is still guaranteed, but is proven against the
        // `communities` vector index (`raptor_wiring.rs`), the only place
        // the vector actually lives.
        anyhow::ensure!(
            a.summary_embedding.is_none(),
            "parity: community {} summary_embedding must be None after KV read-back on storage_a \
             (skip_serializing contract)",
            a.id
        );
        anyhow::ensure!(
            b.summary_embedding.is_none(),
            "parity: community {} summary_embedding must be None after KV read-back on storage_b \
             (skip_serializing contract)",
            b.id
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Split `members` into (community-typed members sorted, leaf member count).
///
/// Community-typed members are those whose IDs appear in `community_id_set`
/// (deterministic blake3-derived). Leaf members are the remainder (random chunk IDs).
fn split_members(
    members: &[Ulid],
    community_id_set: &BTreeSet<Ulid>,
) -> (BTreeMap<[u8; 16], ()>, usize) {
    let mut community_members = BTreeMap::new();
    let mut leaf_count = 0usize;
    for &id in members {
        if community_id_set.contains(&id) {
            community_members.insert(id.to_bytes(), ());
        } else {
            leaf_count += 1;
        }
    }
    (community_members, leaf_count)
}
