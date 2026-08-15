//! RED — chunk-superset floored top-k selection (`floored_top` +
//! `FlooredTopRetriever`).
//!
//! LME N=125 A/B diagnosis (2026-07-29): graph-ON's reranked fact/Navigate
//! candidates outrank mid-value evidence chunks at the FINAL `.top(k)` cut
//! and evict them from the reader context (q251: the date-bearing chunk that
//! was graph-OFF's hit[1] at 0.513 vanished from graph-ON's entire list).
//! Because the chunk legs and cross-encoder scores are identical across arms,
//! reserving a floor of `floor_n` slots for chunk-leg hits makes graph-ON's
//! chunk context a strict superset of graph-OFF's — facts can only ADD
//! context, never displace it.
//!
//! Contract under test:
//! - `floored_top(hits, n, floor_index, floor_n)` returns at most `n` hits in
//!   descending-score order.
//! - At least `min(floor_n, n, available_floor_hits)` of them match the floor
//!   (metadata `"index"` == `floor_index`, or NO index tag — untagged hits
//!   come from legacy chunk-era legs and fail open to the floor).
//! - Remaining slots are filled by global score order regardless of leg.
//! - `hits.len() <= n` → identity (sorted), nothing dropped.

use lunaris_retrieve::{FlooredTopRetriever, RawHit, SourceOp, floored_top};
use serde_json::json;

fn hit(id: &str, score: f32, index: Option<&str>) -> RawHit {
    RawHit {
        id: id.as_bytes().to_vec(),
        score,
        rerank_applied: true,
        degraded: false,
        metadata: match index {
            Some(ix) => json!({ "index": ix }),
            None => json!({}),
        },
        source_op: SourceOp::Reranked,
    }
}

fn ids(hits: &[RawHit]) -> Vec<String> {
    hits.iter().map(|h| String::from_utf8(h.id.clone()).unwrap()).collect()
}

#[test]
fn facts_cannot_displace_chunk_floor() {
    // 6 fact hits outrank every chunk (the q251 displacement shape). With
    // n=10 and a chunk floor of 8, the top-8 chunks MUST all survive; facts
    // get only the 2 unreserved slots.
    let mut hits = Vec::new();
    for i in 0..6 {
        hits.push(hit(&format!("fact{i}"), 0.90 + i as f32 * 0.01, Some("facts")));
    }
    for i in 0..12 {
        hits.push(hit(&format!("chunk{i:02}"), 0.50 - i as f32 * 0.01, Some("chunks")));
    }
    let out = floored_top(hits, 10, "chunks", 8);
    assert_eq!(out.len(), 10);
    let got = ids(&out);
    for i in 0..8 {
        assert!(
            got.contains(&format!("chunk{i:02}")),
            "top-{i} chunk must survive the floor; got {got:?}"
        );
    }
    assert_eq!(
        got.iter().filter(|id| id.starts_with("fact")).count(),
        2,
        "facts compete only for the 2 unreserved slots; got {got:?}"
    );
    // Highest-scoring facts win the open slots.
    assert!(got.contains(&"fact5".to_string()) && got.contains(&"fact4".to_string()));
}

#[test]
fn untagged_hits_count_toward_floor() {
    // Legacy/untagged hits (no metadata "index" key) fail open to the floor —
    // they came from chunk-era legs that predate per-leg tagging.
    let mut hits = vec![hit("fact_hi", 0.99, Some("facts"))];
    for i in 0..4 {
        hits.push(hit(&format!("legacy{i}"), 0.40 - i as f32 * 0.01, None));
    }
    let out = floored_top(hits, 3, "chunks", 3);
    let got = ids(&out);
    assert_eq!(got.len(), 3);
    assert!(
        got.iter().filter(|id| id.starts_with("legacy")).count() == 3,
        "all 3 slots reserved for the (untagged) floor; got {got:?}"
    );
}

#[test]
fn scarce_floor_hits_backfill_from_global_order() {
    // Only 3 chunks exist but the floor asks for 8: take all 3 chunks, then
    // fill the remaining 7 of n=10 slots by global score order (facts).
    let mut hits = Vec::new();
    for i in 0..3 {
        hits.push(hit(&format!("chunk{i}"), 0.30 - i as f32 * 0.01, Some("chunks")));
    }
    for i in 0..20 {
        hits.push(hit(&format!("fact{i:02}"), 0.90 - i as f32 * 0.01, Some("facts")));
    }
    let out = floored_top(hits, 10, "chunks", 8);
    let got = ids(&out);
    assert_eq!(out.len(), 10);
    assert_eq!(got.iter().filter(|id| id.starts_with("chunk")).count(), 3);
    assert_eq!(got.iter().filter(|id| id.starts_with("fact")).count(), 7);
}

#[test]
fn within_budget_returns_all_sorted() {
    let hits = vec![
        hit("a", 0.1, Some("facts")),
        hit("b", 0.9, Some("chunks")),
        hit("c", 0.5, Some("facts")),
    ];
    let out = floored_top(hits, 10, "chunks", 8);
    assert_eq!(ids(&out), vec!["b", "c", "a"], "identity modulo sort when len <= n");
}

#[test]
fn output_is_global_score_order() {
    // Selection must not reorder: floor + fill are picked by score, and the
    // final list stays descending so downstream top-k semantics hold.
    let mut hits = Vec::new();
    for i in 0..5 {
        hits.push(hit(&format!("fact{i}"), 0.95 - i as f32 * 0.1, Some("facts")));
    }
    for i in 0..5 {
        hits.push(hit(&format!("chunk{i}"), 0.90 - i as f32 * 0.1, Some("chunks")));
    }
    let out = floored_top(hits, 6, "chunks", 4);
    let scores: Vec<f32> = out.iter().map(|h| h.score).collect();
    let mut sorted = scores.clone();
    sorted.sort_by(|a: &f32, b: &f32| b.partial_cmp(a).unwrap());
    assert_eq!(scores, sorted, "output must stay descending by score");
    assert_eq!(ids(&out).iter().filter(|id| id.starts_with("chunk")).count(), 4);
}

#[test]
fn floor_larger_than_n_is_capped() {
    // floor_n > n must not overflow the width contract: output is still n.
    let mut hits = Vec::new();
    for i in 0..10 {
        hits.push(hit(&format!("chunk{i}"), 0.5 - i as f32 * 0.01, Some("chunks")));
    }
    hits.push(hit("fact", 0.99, Some("facts")));
    let out = floored_top(hits, 5, "chunks", 50);
    assert_eq!(out.len(), 5);
    assert!(
        ids(&out).iter().all(|id| id.starts_with("chunk")),
        "floor >= n reserves every slot for the floor leg"
    );
}

#[test]
fn retriever_wrapper_exposes_selection_params() {
    // The harness introspects its composed root in tests (the
    // tree_contains_navigate pattern) — the wrapper must expose its
    // parameters for that, and construct from any boxed inner.
    struct Nothing;
    #[async_trait::async_trait]
    impl lunaris_retrieve::Retriever for Nothing {
        async fn retrieve(
            &self,
            _ctx: &lunaris_retrieve::QueryContext,
        ) -> Result<Vec<RawHit>, lunaris_core::LunarisError> {
            Ok(vec![])
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    let r = FlooredTopRetriever::new(Box::new(Nothing), 15, "chunks", 10);
    assert_eq!(r.n(), 15);
    assert_eq!(r.floor_n(), 10);
    assert_eq!(r.floor_index(), "chunks");
}
