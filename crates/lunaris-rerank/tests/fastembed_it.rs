//! `FastembedReranker` integration test matrix — Phase 19-04 Task 2.
//!
//! Gated on **both** `fastembed` and `embedder-it` features (parallel to the
//! `lunaris-embed` Plan 19-02 gate) so the default `cargo test -p lunaris-rerank`
//! run never compiles or executes this file.
//!
//! ## Local-only caveat
//!
//! Tests 1–3 perform a **real ONNX cross-encoder forward pass** of
//! BGE-Reranker-v2-m3 via `fastembed::TextRerank`. First run auto-downloads
//! the ONNX weights from Hugging Face Hub into
//! `$LUNARIS_FASTEMBED_RERANKER_CACHE_DIR` /
//! `~/.cache/lunaris/models/fastembed-reranker/`. Subsequent runs hit the
//! cache. CI integration is deferred — see `19-04-SUMMARY.md`.
//!
//! Test 4 is **offline-safe**: it points fastembed at a path that cannot be
//! a directory (`/dev/null/...`), so `try_new` fails on cache-dir creation
//! **before** any network call.
//!
//! ## Scenarios
//!
//! 1. `real_model_loads_and_reranks_one_batch` — feed a 5-doc batch and a
//!    query, assert preserved-set, sorted-desc, all ids present.
//! 2. `preserved_set_under_repeats` — 4 docs where two share identical text
//!    but different ids; verify the index-keyed re-association does NOT
//!    dedupe by text and each original `id` appears exactly once.
//! 3. `empty_query_or_empty_docs` — (a) empty docs returns `Ok(vec![])`
//!    without touching the model; (b) empty query string does not panic and
//!    returns the docs (in some order).
//! 4. `bogus_cache_dir_returns_actionable_error` — point fastembed at a path
//!    under `/dev/null`; expect `fastembed-reranker: ...` prefix.

#![cfg(all(feature = "fastembed", feature = "embedder-it"))]

use std::path::PathBuf;

use lunaris_core::{LunarisError, StorageError};
use lunaris_rerank::{FastembedReranker, FastembedRerankerOpts, RerankCandidate, Reranker};
use serde_json::json;

/// Build a fresh reranker using the default cache resolution chain. Callers
/// can pre-warm by setting `LUNARIS_FASTEMBED_RERANKER_CACHE_DIR` to a
/// pre-populated directory before running these tests offline.
fn fresh() -> FastembedReranker {
    FastembedReranker::new(FastembedRerankerOpts::default()).expect(
        "FastembedReranker::new — set LUNARIS_FASTEMBED_RERANKER_CACHE_DIR if running offline",
    )
}

fn cand(id: &[u8], text: &str) -> RerankCandidate {
    RerankCandidate { id: id.to_vec(), text: text.to_string(), score: 0.0, metadata: json!({}) }
}

/// Scenario 1 — real model load + preserved set + monotonic-desc scores.
///
/// Five docs from the fastembed README example. The cross-encoder should
/// score the panda doc highest given the query. We verify the trait
/// contract (every input id present in output exactly once + sorted desc)
/// rather than asserting which doc tops the ranking (model-specific).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_model_loads_and_reranks_one_batch() {
    let reranker = fresh();
    assert!(reranker.applies(), "FastembedReranker::applies must be true");

    let docs = vec![
        cand(b"d1", "hi"),
        cand(
            b"d2",
            "The giant panda (Ailuropoda melanoleuca), sometimes called a panda bear or simply panda, is a bear species endemic to China.",
        ),
        cand(b"d3", "panda is animal"),
        cand(b"d4", "i dont know"),
        cand(b"d5", "kind of mammal"),
    ];
    let input_ids: Vec<Vec<u8>> = docs.iter().map(|c| c.id.clone()).collect();

    let out = reranker.rerank("what is panda?", docs).await.expect("rerank");

    // Preserved-set: same count, every input id present exactly once.
    assert_eq!(out.len(), 5, "expected 5 rows, got {}", out.len());
    let mut out_ids: Vec<Vec<u8>> = out.iter().map(|c| c.id.clone()).collect();
    out_ids.sort();
    let mut want_ids = input_ids.clone();
    want_ids.sort();
    assert_eq!(out_ids, want_ids, "output ids must be a permutation of input ids");

    // Monotonic-desc: every adjacent pair sorted by score.
    for w in out.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "scores must be sorted desc, got {} then {}",
            w[0].score,
            w[1].score
        );
    }
}

/// Scenario 2 — preserved-set under text duplicates with distinct ids.
///
/// If the re-association ever switched from `result.index` to
/// `result.document` (matching by text), the two identical-text docs would
/// collapse and one id would be lost. This test pins the index-keyed
/// invariant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preserved_set_under_repeats() {
    let reranker = fresh();
    let docs = vec![
        cand(b"a", "the quick brown fox"),
        cand(b"b", "the quick brown fox"), // same text, different id
        cand(b"c", "lazy dog"),
        cand(b"d", "completely unrelated"),
    ];
    let want_ids: Vec<Vec<u8>> = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()];

    let out = reranker.rerank("fox", docs).await.expect("rerank repeats");

    assert_eq!(out.len(), 4);
    let mut got_ids: Vec<Vec<u8>> = out.iter().map(|c| c.id.clone()).collect();
    got_ids.sort();
    let mut want_sorted = want_ids.clone();
    want_sorted.sort();
    assert_eq!(got_ids, want_sorted, "all 4 distinct ids must survive — including the two identical-text docs");

    // Both repeat-text rows must still have their original ids tagged on
    // distinct output entries.
    let a_seen = out.iter().filter(|c| c.id == b"a".to_vec()).count();
    let b_seen = out.iter().filter(|c| c.id == b"b".to_vec()).count();
    assert_eq!(a_seen, 1, "id 'a' must appear exactly once");
    assert_eq!(b_seen, 1, "id 'b' must appear exactly once");
}

/// Scenario 3 — empty docs (offline) + empty query string (with model).
///
/// (a) `docs.is_empty()` hits the early-return guard *before* any model
/// call, so it runs offline. We construct the reranker first so the test
/// is gated by the same cache requirement as the rest of the file (keeps
/// the matrix's CI semantics uniform). If you need a truly offline empty-
/// docs check, the `#[tokio::test] empty_docs_returns_empty_vec_without_loading_model`
/// unit test in `src/fastembed.rs` covers it via a shim.
///
/// (b) Empty query string still produces a result (fastembed treats it as
/// a zero-length tokenized input; the cross-encoder scores against an empty
/// query — outputs are uniformly low but the function does not panic).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_query_or_empty_docs() {
    let reranker = fresh();

    // (a) empty docs.
    let empty_out = reranker.rerank("anything", Vec::new()).await.expect("empty docs ok");
    assert!(empty_out.is_empty(), "empty docs must return empty vec");

    // (b) empty query — must not panic; output preserves set.
    let docs = vec![cand(b"x", "foo"), cand(b"y", "bar")];
    let out = reranker.rerank("", docs).await.expect("empty query ok");
    assert_eq!(out.len(), 2);
    let mut ids: Vec<Vec<u8>> = out.iter().map(|c| c.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec![b"x".to_vec(), b"y".to_vec()]);
}

/// Scenario 4 — cache-dir error mapping (offline-safe).
///
/// Strategy mirrors the embedder's Plan 19-02 Task 1.4: point fastembed at
/// `/dev/null/...`. On macOS and Linux, `/dev/null` is a character device —
/// `mkdir /dev/null/anything` is `ENOTDIR`. fastembed's `with_cache_dir`
/// triggers a `create_dir_all` inside `try_new`, which fails immediately.
/// Offline-safe; portable across Unix; pins the `fastembed-reranker: ...`
/// error prefix.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bogus_cache_dir_returns_actionable_error() {
    let bogus = PathBuf::from("/dev/null/lunaris-fastembed-reranker-it/cannot-exist");
    let opts =
        FastembedRerankerOpts { cache_dir: Some(bogus), show_download_progress: false };
    let err = FastembedReranker::new(opts)
        .expect_err("constructing under /dev/null/... must fail before any network call");

    match err {
        LunarisError::Storage(StorageError::Backend(msg)) => {
            assert!(
                msg.starts_with("fastembed-reranker"),
                "expected actionable `fastembed-reranker: ...` prefix from anyhow_to_lunaris, got: {msg}"
            );
            eprintln!("// fastembed-reranker reported: {msg}");
        }
        other => panic!("expected LunarisError::Storage(StorageError::Backend(_)), got: {other:?}"),
    }
}
