//! `moon-hybrid-filter-bypass` — RED suite, part 1: `source` Eq / StartsWith +
//! the two Reject scenarios (TASK.md §2).
//!
//! Today these are RED because `compose_query_with_filter` renders the filter
//! into the BM25 text-query, which is a NO-OP on the BM25 branch and ignored by
//! the dense-KNN branch — foreign-source hits leak through RRF fusion. They go
//! GREEN once Moon applies the pushed-down filter to BOTH branches (TASK.md §3
//! CHANGE A/B/G) and `fuse_via_moon_native` passes it via the new SDK param
//! (CHANGE H/I).
//!
//! Run against a live Moon: `MOON_URL=moon://127.0.0.1:6380 cargo test -p
//! lunaris-retrieve --test hybrid_filter`. No Moon → tests SKIP.

mod hybrid_filter_common;

use hybrid_filter_common::{
    ChunkSpec, connect, direct_write_chunks, embedder, id_bytes, ingest_source, recall_hybrid,
    unique_scope,
};
use lunaris_core::storage::types::Filter;
use lunaris_core::HlcClock;

/// Must 1 — a `.filter(Eq source)`'d hybrid recall NEVER returns a foreign
/// source, regardless of RRF rank. PRODUCTION-WIRED: both sources arrive via
/// the real ingest pipeline (`ingest_episode_with_receipt`), so this is the
/// end-to-end discriminator that the production write→read path honours the
/// filter (not just a storage-level fixture).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtered_hybrid_recall_excludes_foreign_source() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let clock = HlcClock::new(0);
    let scope = unique_scope("hfeq");

    // Two sources via the production ingest path.
    let scratchpad_ids = ingest_source(
        &storage,
        &embedder,
        &clock,
        &scope,
        "scratchpad",
        "The current plan is blocked on the embedder cutover and the reranker swap.",
    )
    .await;
    let planning_ids = ingest_source(
        &storage,
        &embedder,
        &clock,
        &scope,
        "planning",
        "Roadmap milestone covers adaptive chunking and the RAPTOR retrieval wiring.",
    )
    .await;
    assert!(!scratchpad_ids.is_empty(), "ingest must produce ≥1 scratchpad chunk");
    assert!(!planning_ids.is_empty(), "ingest must produce ≥1 planning chunk");

    // Large k so the dense-KNN branch returns EVERY chunk today — the leak is
    // then rank-independent: any planning chunk surfacing is a violation.
    let hits = recall_hybrid(
        &storage,
        &embedder,
        &scope,
        "what is the plan",
        50,
        Some(Filter::Eq { field: "source".into(), value: serde_json::json!("scratchpad") }),
    )
    .await;

    assert!(!hits.is_empty(), "filtered recall must still return scratchpad hits");
    let planning = id_bytes(&planning_ids);
    let leaked: Vec<_> = hits.iter().filter(|h| planning.contains(&h.id)).collect();
    assert!(
        leaked.is_empty(),
        "RED: {} planning-source hit(s) leaked past Filter::Eq{{source=scratchpad}} via the \
         dense-KNN branch (foreign source must NEVER appear). hits={} planning_chunks={}",
        leaked.len(),
        hits.len(),
        planning_ids.len(),
    );
}

/// Must 2b — `Filter::StartsWith` on a `source` prefix constrains both
/// branches; the non-matching source is absent from every position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startswith_filter_constrains_both_branches() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfsw");

    // "scratchpad:session-1/2" should match prefix "scratchpad"; "planning" not.
    let ids = direct_write_chunks(
        &storage,
        &embedder,
        &scope,
        &[
            ChunkSpec::new("scratchpad:session-1", None, "session one working notes"),
            ChunkSpec::new("scratchpad:session-1", None, "more session one notes"),
            ChunkSpec::new("scratchpad:session-2", None, "session two scratch buffer"),
            ChunkSpec::new("planning", None, "milestone planning document"),
            ChunkSpec::new("planning", None, "second planning document"),
        ],
    )
    .await;
    // planning chunks are the last two specs.
    let planning = id_bytes(&ids[3..]);

    let hits = recall_hybrid(
        &storage,
        &embedder,
        &scope,
        "notes",
        50,
        Some(Filter::StartsWith { field: "source".into(), prefix: "scratchpad".into() }),
    )
    .await;

    assert!(!hits.is_empty(), "StartsWith recall must return scratchpad* hits");
    let leaked: Vec<_> = hits.iter().filter(|h| planning.contains(&h.id)).collect();
    assert!(
        leaked.is_empty(),
        "RED: {} 'planning' hit(s) leaked past Filter::StartsWith{{source~scratchpad}}. hits={}",
        leaked.len(),
        hits.len(),
    );
}

/// Reject 1 — filter correctness MUST NOT depend on a per-caller post-filter.
/// The `lunaris-retrieve` fuse path has NO post-enforcement (the
/// `WorkingMemory::find` / `recall.rs` guards live in higher layers and are not
/// in this call stack), so a foreign source surviving here proves the DSL
/// boundary itself is broken — exactly the leak this task closes. RED today.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correctness_holds_without_any_per_caller_postfilter() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfnp");

    let ids = direct_write_chunks(
        &storage,
        &embedder,
        &scope,
        &[
            ChunkSpec::new("kept", None, "alpha content one"),
            ChunkSpec::new("kept", None, "alpha content two"),
            ChunkSpec::new("foreign", None, "beta content one"),
            ChunkSpec::new("foreign", None, "beta content two"),
            ChunkSpec::new("foreign", None, "beta content three"),
        ],
    )
    .await;
    let foreign = id_bytes(&ids[2..]);

    // No WorkingMemory / recall.rs guard is reachable from this path.
    let hits = recall_hybrid(
        &storage,
        &embedder,
        &scope,
        "content",
        50,
        Some(Filter::Eq { field: "source".into(), value: serde_json::json!("kept") }),
    )
    .await;

    assert!(!hits.is_empty(), "recall must return 'kept' hits");
    let leaked: Vec<_> = hits.iter().filter(|h| foreign.contains(&h.id)).collect();
    assert!(
        leaked.is_empty(),
        "RED: {} 'foreign' hit(s) survived at the DSL boundary with NO post-filter in the stack — \
         correctness must come from server-side filtering, not a per-caller guard. hits={}",
        leaked.len(),
        hits.len(),
    );
}

/// Reject 2 — dropping the filter push-down must NOT regress the unfiltered
/// path. With `filter = None`, RRF fuses both sources and returns hits from
/// each. This is GREEN today and MUST stay green after the fix (CHANGE A
/// backward-compat: `filter: None` ⇒ identical behaviour). It guards against a
/// fix that accidentally over-filters the no-filter case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_filter_baseline_returns_both_sources() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfbl");

    let ids = direct_write_chunks(
        &storage,
        &embedder,
        &scope,
        &[
            ChunkSpec::new("left", None, "shared topic document one"),
            ChunkSpec::new("left", None, "shared topic document two"),
            ChunkSpec::new("right", None, "shared topic document three"),
            ChunkSpec::new("right", None, "shared topic document four"),
        ],
    )
    .await;
    let left = id_bytes(&ids[..2]);
    let right = id_bytes(&ids[2..]);

    let hits = recall_hybrid(&storage, &embedder, &scope, "shared topic", 50, None).await;

    assert!(!hits.is_empty(), "unfiltered recall must return hits");
    let saw_left = hits.iter().any(|h| left.contains(&h.id));
    let saw_right = hits.iter().any(|h| right.contains(&h.id));
    assert!(
        saw_left && saw_right,
        "no-filter baseline must surface BOTH sources (saw_left={saw_left} saw_right={saw_right}); \
         a regression here means the fix over-filters the unfiltered path",
    );
}
