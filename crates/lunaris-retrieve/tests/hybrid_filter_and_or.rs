//! `moon-hybrid-filter-bypass` — RED suite, part 3: compound filters (TASK.md
//! §2 Must 2c And, Must 2d Or, Must 2e ValidTimeRange / §3 CHANGE I full-tree).
//!
//! These assert the SERVER-SIDE translation is correctness-complete (the
//! `lunaris-retrieve` layer has no post-filter to mask a partial result). Today
//! every compound filter leaks foreign hits through the dense-KNN branch (RED);
//! after CHANGE A/B/I they constrain both branches (GREEN).
//!
//! Scope note (frozen-contract clarification, 2026-06-12): the §2 And scenario
//! originally read `source AND chunk_type`, but `chunk_type` is NOT an indexed
//! FT field and §1 Must scopes the fix to "indexed fields". The And test below
//! therefore intersects the two REAL indexed fields — `source` (TAG) ∩
//! `valid_time` (NUMERIC) — which is the stronger heterogeneous-leaf test
//! (bitmap intersection of a TAG allowlist and a numeric-range allowlist, the
//! riskiest CHANGE B path). `valid_time` is populated by DIRECT WRITE here
//! because the production ingest path does not yet emit `valid_time_ms`
//! (pipeline.rs:328-334) — a separate gap recorded in TASK.md §4/§7.

mod hybrid_filter_common;

use hybrid_filter_common::{
    ChunkSpec, VT_2025_MS, VT_2026_MS, connect, direct_write_chunks, embedder, hlc_at, id_bytes,
    recall_hybrid, unique_scope,
};
use lunaris_core::storage::types::Filter;

/// Must 2c — `And([Eq source, ValidTimeRange])` keeps only hits matching BOTH
/// the TAG and the NUMERIC leaf; hits matching only one are absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn and_filter_source_and_valid_time_constrains_both() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfand");

    // 3 kept (source=a ∧ vt=2025); 3 rejected-by-time (a ∧ 2026); 3
    // rejected-by-source (b ∧ 2025).
    let mut specs = Vec::new();
    for i in 0..3 {
        specs.push(ChunkSpec::new("a", Some(VT_2025_MS), &format!("a-2025 doc {i}")));
    }
    for i in 0..3 {
        specs.push(ChunkSpec::new("a", Some(VT_2026_MS), &format!("a-2026 doc {i}")));
    }
    for i in 0..3 {
        specs.push(ChunkSpec::new("b", Some(VT_2025_MS), &format!("b-2025 doc {i}")));
    }
    let ids = direct_write_chunks(&storage, &embedder, &scope, &specs).await;
    let rejected = id_bytes(&ids[3..]); // everything except the first 3 (a ∧ 2025)

    let filter = Filter::And(vec![
        Filter::Eq { field: "source".into(), value: serde_json::json!("a") },
        Filter::ValidTimeRange {
            after: Some(hlc_at(VT_2025_MS - 10_000_000_000)),
            before: Some(hlc_at(VT_2026_MS - 10_000_000_000)),
        },
    ]);

    let hits = recall_hybrid(&storage, &embedder, &scope, "doc", 50, Some(filter)).await;

    assert!(!hits.is_empty(), "And recall must return the a∧2025 hits");
    let leaked: Vec<_> = hits.iter().filter(|h| rejected.contains(&h.id)).collect();
    assert!(
        leaked.is_empty(),
        "RED: {} hit(s) matching only one And-leaf leaked (a∧2026 or b∧2025); a conjunction must \
         exclude both. hits={}",
        leaked.len(),
        hits.len(),
    );
}

/// Must 2d — `Or([Eq alpha, Eq gamma])` accepts either disjunct's source and
/// rejects the third; proves the server union is correctness-complete (a
/// dropped disjunct would silently return a subset — forbidden by CHANGE I).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn or_filter_accepts_either_rejects_third() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfor");

    let ids = direct_write_chunks(
        &storage,
        &embedder,
        &scope,
        &[
            ChunkSpec::new("alpha", None, "alpha document one"),
            ChunkSpec::new("alpha", None, "alpha document two"),
            ChunkSpec::new("beta", None, "beta document one"),
            ChunkSpec::new("beta", None, "beta document two"),
            ChunkSpec::new("gamma", None, "gamma document one"),
            ChunkSpec::new("gamma", None, "gamma document two"),
        ],
    )
    .await;
    let beta = id_bytes(&ids[2..4]);

    let filter = Filter::Or(vec![
        Filter::Eq { field: "source".into(), value: serde_json::json!("alpha") },
        Filter::Eq { field: "source".into(), value: serde_json::json!("gamma") },
    ]);

    let hits = recall_hybrid(&storage, &embedder, &scope, "document", 50, Some(filter)).await;

    assert!(!hits.is_empty(), "Or recall must return alpha/gamma hits");
    let leaked: Vec<_> = hits.iter().filter(|h| beta.contains(&h.id)).collect();
    assert!(
        leaked.is_empty(),
        "RED: {} 'beta' hit(s) leaked past Or([alpha, gamma]); the third source must be absent. \
         hits={}",
        leaked.len(),
        hits.len(),
    );
}

/// Must 2e — `ValidTimeRange` bounds both branches: chunks whose valid_time is
/// entirely outside the window are absent. Single source isolates the time
/// axis.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_time_range_bounds_both_branches() {
    let Some(storage) = connect().await else { return };
    let embedder = embedder();
    let scope = unique_scope("hfvt");

    // 4 in-window (2025), 4 out-of-window (2026), all source="notes".
    let mut specs = Vec::new();
    for i in 0..4 {
        specs.push(ChunkSpec::new("notes", Some(VT_2025_MS), &format!("2025 note {i}")));
    }
    for i in 0..4 {
        specs.push(ChunkSpec::new("notes", Some(VT_2026_MS), &format!("2026 note {i}")));
    }
    let ids = direct_write_chunks(&storage, &embedder, &scope, &specs).await;
    let out_of_window = id_bytes(&ids[4..]); // the 2026 chunks

    let filter = Filter::ValidTimeRange {
        after: Some(hlc_at(VT_2025_MS - 10_000_000_000)),
        before: Some(hlc_at(VT_2026_MS - 10_000_000_000)),
    };

    let hits = recall_hybrid(&storage, &embedder, &scope, "note", 50, Some(filter)).await;

    assert!(!hits.is_empty(), "ValidTimeRange recall must return in-window hits");
    let leaked: Vec<_> = hits.iter().filter(|h| out_of_window.contains(&h.id)).collect();
    assert!(
        leaked.is_empty(),
        "RED: {} out-of-window (2026) hit(s) leaked past a 2025-only ValidTimeRange. hits={}",
        leaked.len(),
        hits.len(),
    );
}
