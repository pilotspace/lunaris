//! F21 (boundary half) — the HYBRID fuse leg must honour the half-open
//! `[after, before)` contract, same as the vector and keyword legs.
//!
//! ## Why this file exists separately
//!
//! `lunaris-storage-moon`'s `valid_time_half_open` suite pins
//! `vector_search` and `keyword_search`. Both went green while the
//! TypeScript parity suite stayed red at `expected 7 to be 6`, because the
//! `DocumentCorpus` recipes take NEITHER of those paths: `TemporalQuery`
//! composes `Vector AND Keyword |> fuse_rrf`, which routes through
//! `fusion.rs::filter_node_to_hybrid` into Moon's native `HybridFilter` —
//! a FIFTH render of this filter, with its own inclusive `max`.
//!
//! Five sites render `Filter::ValidTimeRange`, and a guard covering four of
//! them reads exactly like a guard covering all five. The end-to-end SDK run
//! is what caught it; this test is what will catch it next time.
//!
//! ## What it asserts
//!
//! Rows at `lo`, strictly inside, and at `hi`, against the window
//! `[lo, hi)`. The lower bound and the inside row are asserted alongside the
//! upper bound so "the upper bound excludes" cannot be satisfied by a filter
//! that has quietly become too narrow, or by one that matched nothing.

mod hybrid_filter_common;

use hybrid_filter_common::{
    ChunkSpec, connect, direct_write_chunks, embedder, hlc_at, recall_hybrid, unique_scope,
};
use lunaris_core::storage::types::Filter;

const BEFORE_LO: u64 = 1_736_467_199_999;
const LO: u64 = 1_736_467_200_000; // 2025-01-10T00:00:00Z
const INSIDE: u64 = 1_736_899_200_000; // 2025-01-15T00:00:00Z
const HI: u64 = 1_736_985_600_000; // 2025-01-16T00:00:00Z

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_fuse_treats_the_upper_bound_as_exclusive() {
    let Some((_moon, storage)) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("f21-hybrid");

    let specs = [
        ChunkSpec::new("timeline:events/", Some(BEFORE_LO), "timeline event outside below"),
        ChunkSpec::new("timeline:events/", Some(LO), "timeline event on the lower bound"),
        ChunkSpec::new("timeline:events/", Some(INSIDE), "timeline event inside the window"),
        ChunkSpec::new("timeline:events/", Some(HI), "timeline event on the upper bound"),
    ];
    let ids = direct_write_chunks(&storage, &embedder, &scope, &specs).await;
    let (before_lo, at_lo, inside, at_hi) = (
        ids[0].to_bytes().to_vec(),
        ids[1].to_bytes().to_vec(),
        ids[2].to_bytes().to_vec(),
        ids[3].to_bytes().to_vec(),
    );

    let filter = Filter::ValidTimeRange { after: Some(hlc_at(LO)), before: Some(hlc_at(HI)) };
    let hits = recall_hybrid(&storage, &embedder, &scope, "timeline", 50, Some(filter)).await;
    let got: Vec<Vec<u8>> = hits.iter().map(|h| h.id.clone()).collect();
    let has = |id: &Vec<u8>| got.iter().any(|g| g == id);

    // Vacuity floor first: with an empty result every "must be absent"
    // assertion below would pass for the wrong reason.
    assert!(
        has(&inside),
        "the row squarely INSIDE the window is missing, so this test proves nothing about \
         either boundary. got {} hits",
        got.len()
    );
    assert!(
        has(&at_lo),
        "the lower bound must be INCLUSIVE — `[after, ...`. A filter that excludes it is too \
         narrow at both ends, which would make the upper-bound assertion below pass for the \
         wrong reason. got {} hits",
        got.len()
    );
    assert!(
        !has(&before_lo),
        "a row one millisecond BELOW the window came back. got {} hits",
        got.len()
    );
    assert!(
        !has(&at_hi),
        "F21: the hybrid fuse leg returned a row sitting exactly ON the upper bound. \
         `Filter::ValidTimeRange` is half-open `[after, before)`. Moon's `HybridFilter::Numeric` \
         is inclusive on both ends, so `fusion.rs::filter_node_to_hybrid` must render `max` as \
         `hi - 1` — exact, because `valid_time` is only ever written from an integer `wall_ms`. \
         got {} hits",
        got.len()
    );
}
