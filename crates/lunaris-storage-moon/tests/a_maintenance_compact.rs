//! moon-v051-perf-exploit W1-3 — live-Moon proof of
//! `lunaris_storage_moon::vector::maybe_compact_after_bulk_ingest`, the free
//! function backing the (not-yet-wired, see this workstream's summary)
//! `StoragePort::maintenance_hint` Moon override.
//!
//! Calls the function DIRECTLY (it is `pub`, not routed through the trait —
//! `impl StoragePort for MoonStorage` lives in `lib.rs`, outside this
//! workstream's file ownership) against a real Moon, and asserts via
//! `FT.INFO`'s `graph_segments` counter (Moon HQ-1/R5 observability) that:
//!
//! 1. below `LUNARIS_MOON_COMPACT_MIN`, the mutable segment is left alone
//!    (no FT.COMPACT issued — `graph_segments == 0`), and
//! 2. at/above the gate, FT.COMPACT actually runs and the vectors move into
//!    a compacted (HNSW) segment (`graph_segments >= 1`).
//!
//! ## Vendor-source correction vs `tmp/moon-perf-context.md`
//!
//! The shared context doc states "`FT.COMPACT` silently no-ops when mutable
//! segment < compact_threshold". Reading the CURRENT vendored source
//! (`vendor/moon/src/command/vector_search/ft_admin.rs::ft_compact`) shows
//! the opposite: the explicit `FT.COMPACT <name>` command calls
//! `idx.force_compact()` UNCONDITIONALLY, bypassing `compact_threshold`
//! entirely (comment: "FT.COMPACT is explicit user intent: compact
//! unconditionally, ignoring threshold"). The no-op case belongs to a
//! DIFFERENT internal call path (`try_compact`, used by background
//! autocompact), not the wire command this maintenance hint issues. This
//! test's below-threshold case therefore asserts "our GATE didn't call
//! FT.COMPACT at all" (`graph_segments == 0` because we returned early),
//! not "we called FT.COMPACT and Moon silently no-op'd it" — a materially
//! different (and, per the vendored source, more accurate) claim. See this
//! workstream's summary for the full discrepancy writeup.
//!
//! ```bash
//! MOON_URL=moon://localhost:7801 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test a_maintenance_compact -- --nocapture
//! ```

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_moon::vector::{
    maybe_compact_after_bulk_ingest, maybe_compact_after_bulk_ingest_with_min,
};
use serde_json::json;

mod common;
const DIM: usize = 768;

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:7801".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&moon_url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            common::note_moon_unreachable(e);
            None
        }
    }
}

fn det_vec(seed: u64) -> Vec<f32> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(DIM);
    for _ in 0..DIM {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push(((x >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0);
    }
    v
}

fn doc_id(i: usize) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&(i as u64).to_be_bytes());
    id[15] = b'M';
    id
}

async fn seed(moon: &MoonStorage, scope: &Scope, n: usize) {
    for batch in (0..n).collect::<Vec<_>>().chunks(100) {
        let ops: Vec<WriteOp> = batch
            .iter()
            .map(|&i| WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: doc_id(i),
                embedding: det_vec(i as u64 + 1),
                metadata: json!({"text": format!("doc {i}"), "source": "maint-eval"}),
            })
            .collect();
        moon.atomic_write(scope, &ops).await.expect("seed write");
    }
}

async fn graph_segments(moon: &MoonStorage, scope: &Scope) -> i64 {
    let idx_name = format!("lunaris_{}_chunks_idx", scope.as_str());
    let typed = moon.client().typed();
    let info = typed.vector().index_info(&idx_name).await.expect("FT.INFO must succeed");
    info.extra.get("graph_segments").and_then(|s| s.parse().ok()).unwrap_or(-1)
}

/// §1 — below `LUNARIS_MOON_COMPACT_MIN` (default 512), the maintenance hint
/// is a true no-op: no FT.COMPACT round trip, `graph_segments` stays 0
/// (everything lives in the brute-force mutable segment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_gate_leaves_mutable_segment_uncompacted() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = Scope::new(format!("maint-below-{}", ulid::Ulid::new())).expect("valid scope");
    seed(&moon, &scope, 20).await;

    assert_eq!(
        graph_segments(&moon, &scope).await,
        0,
        "fresh index must start with 0 compacted (immutable) segments"
    );

    maybe_compact_after_bulk_ingest(moon.client(), &scope, 20)
        .await
        .expect("maintenance_hint must not error even when it's a no-op");

    assert_eq!(
        graph_segments(&moon, &scope).await,
        0,
        "20 upserts is below the default 512 gate — FT.COMPACT must NOT have been issued"
    );
}

/// §2 DISCRIMINATOR — at/above the gate (forced low via
/// `LUNARIS_MOON_COMPACT_MIN=5` for a fast test), the hint issues FT.COMPACT
/// on the scope's chunks index and the vectors move into a real (HNSW)
/// segment.
///
/// The gate is passed in as an ARGUMENT, not lowered via
/// `LUNARIS_MOON_COMPACT_MIN`. The earlier version of this test set that
/// variable with `std::env::set_var`, which is process-wide, and raced its own
/// sibling: `§1` above runs concurrently and asserts that 20 upserts stay below
/// the default 512 gate, but while this test held the gate at 5 that call saw
/// 20 >= 5, compacted, and `§1` failed with `left: 1, right: 0`. The old
/// comment here argued the mutation was safe because grep found no other site
/// naming the variable — but `§1` never names it; it reads it the way every
/// caller does, through `maybe_compact_after_bulk_ingest`. Shared mutable
/// process state is not made safe by the absence of a grep hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_or_above_gate_compacts_into_a_real_segment() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = Scope::new(format!("maint-above-{}", ulid::Ulid::new())).expect("valid scope");
    seed(&moon, &scope, 10).await;

    assert_eq!(graph_segments(&moon, &scope).await, 0, "starts uncompacted");

    maybe_compact_after_bulk_ingest_with_min(moon.client(), &scope, 10, 5)
        .await
        .expect("maintenance_hint must succeed at/above the gate");

    let segments = graph_segments(&moon, &scope).await;
    assert!(
        segments >= 1,
        "10 upserts >= gate(5) must have triggered FT.COMPACT — expected graph_segments >= 1, \
         got {segments} (Moon's FT.COMPACT force-compacts unconditionally per vendor source; \
         see this file's module doc for the vendor-source correction)"
    );

    // Recall must still work post-compaction (compacted segments answer via
    // HNSW + exact f16 rerank, not brute force).
    let q = det_vec(1);
    let hits = moon
        .vector_search(&scope, "chunks", &q, 5, None, None, false)
        .await
        .expect("vector_search after compaction must succeed");
    assert!(!hits.is_empty(), "recall must still return hits after compaction");
}
