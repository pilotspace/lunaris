//! Phase B acceptance (llama.cpp-only cutover) — the PRODUCTION resolution
//! path (`Lunaris::open` → `resolve_embedder` / `resolve_reranker`) serves
//! the llama.cpp backends when `--features llamacpp` is on.
//!
//! Built ≠ wired (CLAUDE.md / v0.6 lesson): the backends existing in
//! `lunaris-llamacpp` proves nothing about `open()` using them. The
//! discriminator here is BEHAVIORAL, not log-based:
//!
//! - `LUNARIS_EMBEDDER_DIR` / `LUNARIS_RERANKER_DIR` point at EMPTY temp
//!   dirs, so every candle-native path degrades to `NoopEmbedder` (zero
//!   vectors) / `NoopReranker` (`applies() == false`).
//! - The staged GGUFs are reachable (env override or `~/.lunaris/models/`
//!   defaults) — ONLY the llamacpp resolution step can produce a real
//!   embedder/reranker under this env.
//! - Real = non-zero L2-normalized 768-d embeddings; reranker that
//!   `applies()` and puts the canonical bge panda pair in the right order.
//!
//! One `#[tokio::test]` for both halves: backend resolution logs once per
//! process and env mutation must not race.

#![cfg(feature = "llamacpp")]
// `std::env::set_var` is `unsafe` in Rust 2024; permitted at the
// test-binary level only (mirrors `default_flip.rs`). The single-test
// layout means no intra-binary races, and the guard pattern keeps env
// writes before any `.await`.
#![allow(unsafe_code)]

use std::path::PathBuf;

use lunaris::{Lunaris, RerankCandidate};
use lunaris_test_harness::open_test_store;

fn staged(name: &str) -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".lunaris/models").join(name))
        .filter(|p| p.exists())
}

#[tokio::test]
async fn production_open_resolves_llamacpp_embedder_and_reranker() {
    let (Some(embed_gguf), Some(rerank_gguf)) = (
        std::env::var_os("LUNARIS_EMBEDDER_GGUF")
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| staged("granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")),
        std::env::var_os("LUNARIS_RERANKER_GGUF")
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| staged("bge-reranker-v2-m3.Q5_K_M.gguf")),
    ) else {
        eprintln!(
            "[skip] production_open_resolves_llamacpp_embedder_and_reranker — GGUFs not staged"
        );
        return;
    };

    // Starve every candle path: empty model dirs mean FP16/FP32 loading
    // fails and (in a candle build) resolution would fall back to
    // Noop{Embedder,Reranker}. If this build serves REAL vectors + a REAL
    // rerank, the llamacpp step is wired into the production chain.
    let empty = std::env::temp_dir().join(format!("lunaris-llamacpp-wired-{}", std::process::id()));
    std::fs::create_dir_all(&empty).expect("create empty model dir");
    // SAFETY-NOTE: single-test binary, set before any `.await`.
    unsafe {
        std::env::set_var("LUNARIS_EMBEDDER_DIR", &empty);
        std::env::set_var("LUNARIS_RERANKER_DIR", &empty);
        std::env::set_var("LUNARIS_EMBEDDER_GGUF", &embed_gguf);
        std::env::set_var("LUNARIS_RERANKER_GGUF", &rerank_gguf);
    }

    // 0.7.0 port off `memory://`. The constructor stays `Lunaris::open` (no
    // embedder argument) because the whole point is that the RESOLVER picks the
    // llama.cpp embedder — the harness's `open_test_engine()` would substitute
    // a StubEmbedder and the assertions below would prove nothing.
    let store = open_test_store().await;
    let handle = Lunaris::open(store.url()).await.expect("open test store");

    // Embedder half: real 768-d, unit-norm, non-degenerate.
    let embedder = handle.embedder();
    assert_eq!(embedder.dim(), 768, "llamacpp granite-r2 must report 768-d");
    let rows = embedder
        .embed_batch(&["a tiny memo about the standup", "quarterly revenue numbers"])
        .await
        .expect("embed through the production-resolved embedder");
    for row in &rows {
        assert_eq!(row.len(), 768);
        let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "NoopEmbedder emits zero vectors — a real llamacpp embedder must be \
             L2-normalized, got norm {norm}"
        );
    }
    let cos: f32 = rows[0].iter().zip(&rows[1]).map(|(a, b)| a * b).sum();
    assert!(cos < 0.97, "distinct prompts must not collapse (cosine {cos})");

    // Reranker half: applies() promises a real cross-encoder, and the
    // canonical bge panda pair must order correctly through the
    // production-resolved handle.
    let reranker = handle.reranker();
    assert!(
        reranker.applies(),
        "candle fallback under an empty model dir is NoopReranker \
         (applies()==false) — llamacpp resolution must win"
    );
    let docs = vec![
        RerankCandidate {
            id: vec![0],
            text: "hi".into(),
            score: 0.0,
            metadata: serde_json::Value::Null,
        },
        RerankCandidate {
            id: vec![1],
            text: "The giant panda (Ailuropoda melanoleuca), sometimes called a panda bear \
                   or simply panda, is a bear species endemic to China."
                .into(),
            score: 0.0,
            metadata: serde_json::Value::Null,
        },
    ];
    let out = reranker.rerank("what is panda?", docs).await.expect("rerank");
    assert_eq!(out[0].id, vec![1], "the relevant doc must rank first");
    assert!(
        out[0].score > 0.9 && out[1].score < 0.1,
        "canonical pair must discriminate (FP32 ref ≈0.997/0.0003), got {} / {}",
        out[0].score,
        out[1].score
    );
}
