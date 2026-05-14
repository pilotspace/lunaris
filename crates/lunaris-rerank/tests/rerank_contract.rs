//! Reranker contract tests.
//!
//! Runs on every default `cargo test -p lunaris-rerank`. The v0.4 N-03
//! cutover deleted `BgeRerankerV2M3`; the bge cross-encoder + its live
//! integration test now live in `lunaris-rerank-native`. This contract
//! suite is the canonical trait + Noop coverage that survives the cutover.

use std::sync::Arc;

use lunaris_rerank::{NoopReranker, RerankCandidate, Reranker};
use serde_json::json;

fn cand(id: &[u8], text: &str, score: f32) -> RerankCandidate {
    RerankCandidate { id: id.to_vec(), text: text.to_string(), score, metadata: json!({}) }
}

#[tokio::test]
async fn noop_returns_input_unchanged() {
    let docs = vec![
        cand(b"a", "first doc", 0.1),
        cand(b"b", "second doc", 0.5),
        cand(b"c", "third doc", 0.3),
        cand(b"d", "fourth doc", 0.9),
        cand(b"e", "fifth doc", 0.2),
    ];
    let out = NoopReranker.rerank("any query", docs).await.unwrap();
    assert_eq!(out.len(), 5);
    // NoopReranker MUST preserve input order (does NOT re-sort).
    assert_eq!(out[0].id, b"a".to_vec());
    assert_eq!(out[1].id, b"b".to_vec());
    assert_eq!(out[2].id, b"c".to_vec());
    assert_eq!(out[3].id, b"d".to_vec());
    assert_eq!(out[4].id, b"e".to_vec());
    assert!(!NoopReranker.applies(), "NoopReranker MUST report applies()=false");
}

#[tokio::test]
async fn reranker_is_dyn_compat() {
    fn _check<T: Reranker + ?Sized>() {}
    _check::<dyn Reranker>();
    let r: Arc<dyn Reranker> = Arc::new(NoopReranker);
    assert!(!r.applies());
    let docs = vec![cand(b"x", "x", 0.0)];
    let out = r.rerank("q", docs).await.unwrap();
    assert_eq!(out.len(), 1);
}
