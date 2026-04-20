//! Reranker contract tests.
//!
//! These tests run on every default `cargo test -p lunaris-rerank` (no live
//! model file required). The `rerank-it`-gated test exercises a real
//! bge-reranker-v2-m3 forward pass against a fixed five-document set; it
//! SKIPs gracefully when `LUNARIS_RERANK_BGE_PATH` is unset (matches the
//! Plan 02-01 `embedder-it` skip pattern).

use std::sync::Arc;

use lunaris_rerank::{NoopReranker, RerankCandidate, Reranker};
use serde_json::json;

fn cand(id: &[u8], text: &str, score: f32) -> RerankCandidate {
    RerankCandidate {
        id: id.to_vec(),
        text: text.to_string(),
        score,
        metadata: json!({}),
    }
}

#[cfg(feature = "candle")]
#[tokio::test]
async fn missing_weights_returns_actionable_error() {
    use lunaris_rerank::{BgeRerankerV2M3, BgeRerankerV2M3Opts};
    let bogus = std::env::temp_dir().join("lunaris-bge-no-such-cache-xyz-2026-04-20");
    let _ = std::fs::remove_dir_all(&bogus);
    let res = BgeRerankerV2M3::try_new(BgeRerankerV2M3Opts {
        model_path: Some(bogus.clone()),
        device: candle_core::Device::Cpu,
    })
    .await;
    let err = res.expect_err("missing path must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("bge-reranker-v2-m3 weights missing"),
        "error must be the actionable cache-miss message; got: {msg}"
    );
    assert!(msg.contains("BAAI/bge-reranker-v2-m3"), "error must mention model id; got: {msg}");
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

#[cfg(all(feature = "candle", feature = "rerank-it"))]
#[tokio::test]
async fn bge_loads_real_model() {
    use lunaris_rerank::BgeRerankerV2M3;
    let path = match std::env::var("LUNARIS_RERANK_BGE_PATH") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "SKIP bge_loads_real_model — set LUNARIS_RERANK_BGE_PATH to the bge-reranker-v2-m3 cache dir"
            );
            return;
        }
    };
    let r = BgeRerankerV2M3::try_new_from_path(path.into())
        .await
        .expect("real model must load when LUNARIS_RERANK_BGE_PATH points at a real cache");
    assert!(r.applies());

    let docs = vec![
        cand(b"berlin", "Berlin is the capital of Germany.", 0.5),
        cand(b"paris", "Paris is the capital of France.", 0.4),
        cand(b"rome", "Rome is the capital of Italy.", 0.3),
        cand(b"madrid", "Madrid is the capital of Spain.", 0.2),
        cand(b"london", "London is the capital of the United Kingdom.", 0.1),
    ];
    let out = r.rerank("what is the capital of France?", docs).await.unwrap();
    assert_eq!(out.len(), 5);
    // The doc mentioning Paris MUST land in position 0 — that's the entire
    // point of using a cross-encoder over a bi-encoder.
    assert_eq!(out[0].id, b"paris".to_vec(), "real bge-reranker should rank Paris top");
}
