//! `moon-hybrid-filter-bypass` — RED suite, part 2: k-starvation (TASK.md §2
//! Must 3 / §3 CHANGE C).
//!
//! When a filter is present the fix narrows the raw candidate window on each
//! branch, so it MUST fan `k_per_stream` out (CHANGE C: `max(default, 5*top_k,
//! 60)`) or the fused top-k silently starves below what a filtered
//! single-branch search would return. This test pins that guarantee with an
//! ALL-MATCHING corpus: every chunk satisfies the filter, so a correct fix must
//! still fill `k`.
//!
//! Status: GREEN today (the filter is a no-op, so an all-same-source corpus is
//! unaffected) and GREEN after a CORRECT fix; it goes RED only if a naive fix
//! filters the candidate window without the CHANGE C fan-out (starvation). It
//! is the guard that keeps the k-starvation Must honest through the build.

mod hybrid_filter_common;

use hybrid_filter_common::{ChunkSpec, connect, direct_write_chunks, embedder, id_bytes, recall_hybrid, unique_scope};
use lunaris_core::storage::types::Filter;

/// Must 3 — filtered hybrid top-k is not silently starved below
/// `min(k, total_matching)`. Corpus: 20 chunks all from "scratchpad"; k = 10.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtered_top_k_is_not_starved() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfks");

    let specs: Vec<ChunkSpec> = (0..20)
        .map(|i| ChunkSpec::new("scratchpad", None, &format!("scratchpad working note number {i}")))
        .collect();
    let ids = direct_write_chunks(&storage, &embedder, &scope, &specs).await;
    let all = id_bytes(&ids);
    assert_eq!(all.len(), 20, "fixture must write 20 distinct chunks");

    let k = 10;
    let hits = recall_hybrid(
        &storage,
        &embedder,
        &scope,
        "working note",
        k,
        Some(Filter::Eq { field: "source".into(), value: serde_json::json!("scratchpad") }),
    )
    .await;

    // Every hit must be a real scratchpad chunk (no junk), and the window must
    // be filled to min(k, total_matching) = 10 — not starved by the filter.
    assert!(
        hits.iter().all(|h| all.contains(&h.id)),
        "all hits must be scratchpad chunks from the fixture",
    );
    assert_eq!(
        hits.len(),
        k,
        "filtered top-k must equal min(k, total_matching)=min(10,20)=10; got {} — a count below k \
         on an all-matching corpus is the k-starvation regression CHANGE C must prevent",
        hits.len(),
    );
}
