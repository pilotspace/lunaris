//! Phase A1 acceptance test for the llama.cpp reranker (cutover milestone —
//! see memory/ADR: llama.cpp becomes the only inference runtime; the
//! reranker is unknown #1).
//!
//! Gated behind `--features llamacpp` and the staged bge-reranker-v2-m3
//! GGUF; skips (does not fail) when the artifact is missing — same
//! convention as `llamacpp_smoke.rs`.
//!
//! Contract under test (parity with the retired candle reranker's output
//! contract, so the umbrella can swap implementations 1:1):
//! - sigmoid scores in [0, 1], one per candidate, positionally derivable;
//! - the canonical bge panda pair discriminates: relevant > 0.9,
//!   irrelevant < 0.1 (FP32 reference ≈ 0.997 / ≈ 0.0003 — a Q5 quant that
//!   can't clear these loose bounds is broken, not drifted);
//! - the `lunaris_rerank::Reranker` impl returns exactly `docs.len()`
//!   candidates sorted score-desc with `applies() == true`;
//! - an embedder and a reranker COEXIST in one process (shared
//!   `LlamaBackend` — llama.cpp's backend init is once-per-process, so two
//!   models must not each own it).

#![cfg(feature = "llamacpp")]

use std::path::PathBuf;

use lunaris_llamacpp::{LlamaCppReranker, LlamaCppRerankerOpts};
use lunaris_rerank::{RerankCandidate, Reranker};

const QUERY: &str = "what is panda?";
const RELEVANT: &str = "The giant panda (Ailuropoda melanoleuca), sometimes called a panda bear \
                        or simply panda, is a bear species endemic to China.";
const IRRELEVANT: &str = "hi";

fn reranker_gguf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_RERANKER_GGUF").map(PathBuf::from) {
        return Some(p);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf"))
        .filter(|p| p.exists())
}

fn embedder_gguf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_EMBEDDER_GGUF").map(PathBuf::from) {
        return Some(p);
    }
    std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
        })
        .filter(|p| p.exists())
}

fn open_reranker(gguf_path: PathBuf) -> LlamaCppReranker {
    LlamaCppReranker::open(LlamaCppRerankerOpts {
        gguf_path,
        n_gpu_layers: if cfg!(feature = "metal") { u32::MAX } else { 0 },
        ..Default::default()
    })
    .expect("open llama.cpp reranker")
}

fn candidate(id: u8, text: &str) -> RerankCandidate {
    RerankCandidate {
        id: vec![id],
        text: text.to_string(),
        score: 0.0,
        metadata: serde_json::Value::Null,
    }
}

#[test]
fn rank_scores_discriminate_canonical_pairs() {
    let Some(gguf) = reranker_gguf() else {
        eprintln!(
            "[skip] rank_scores_discriminate_canonical_pairs — bge GGUF not staged \
             (set LUNARIS_RERANKER_GGUF)"
        );
        return;
    };
    let reranker = open_reranker(gguf);

    let scores =
        reranker.score_blocking(QUERY, &[IRRELEVANT, RELEVANT, IRRELEVANT]).expect("score");
    assert_eq!(scores.len(), 3, "one score per candidate");
    for s in &scores {
        assert!((0.0..=1.0).contains(s), "sigmoid contract violated: {s}");
    }
    assert!(scores[1] > 0.9, "relevant pair must score >0.9 (FP32 ref ≈0.997), got {}", scores[1]);
    assert!(
        scores[0] < 0.1 && scores[2] < 0.1,
        "irrelevant pairs must score <0.1 (FP32 ref ≈0.0003), got {} / {}",
        scores[0],
        scores[2]
    );
    // Same pair at different positions in the pack must agree closely
    // (ragged packing is position-sensitive at ~1e-3; 1e-2 on a sigmoid
    // output is the generous ceiling).
    assert!(
        (scores[0] - scores[2]).abs() < 1e-2,
        "identical pairs diverged across pack positions: {} vs {}",
        scores[0],
        scores[2]
    );
}

#[tokio::test]
async fn trait_rerank_sorts_desc_and_preserves_set() {
    let Some(gguf) = reranker_gguf() else {
        eprintln!("[skip] trait_rerank_sorts_desc_and_preserves_set — bge GGUF not staged");
        return;
    };
    let reranker = open_reranker(gguf);
    assert!(reranker.applies(), "model-backed reranker must report applies() == true");

    let docs = vec![
        candidate(0, IRRELEVANT),
        candidate(1, RELEVANT),
        candidate(2, "pandas eat bamboo in the mountain forests of Sichuan."),
    ];
    let out = reranker.rerank(QUERY, docs).await.expect("rerank");

    assert_eq!(out.len(), 3, "reranker must return exactly docs.len() candidates");
    assert!(out.windows(2).all(|w| w[0].score >= w[1].score), "output must be sorted score-desc");
    let mut ids: Vec<u8> = out.iter().map(|c| c.id[0]).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1, 2], "the input set must be preserved, only re-ordered");
    assert_eq!(out[0].id, vec![1], "the exact-answer doc must rank first");
}

#[tokio::test]
async fn embedder_and_reranker_coexist_in_one_process() {
    let (Some(rerank_gguf), Some(embed_gguf)) = (reranker_gguf(), embedder_gguf()) else {
        eprintln!("[skip] embedder_and_reranker_coexist_in_one_process — GGUF(s) not staged");
        return;
    };

    // Order matters for the regression this pins: the SECOND open used to
    // fail with BackendAlreadyInitialized when each model owned the backend.
    use lunaris_core::Embedder;
    let embedder =
        lunaris_llamacpp::LlamaCppEmbedder::open(lunaris_llamacpp::LlamaCppEmbedderOpts {
            gguf_path: embed_gguf,
            n_gpu_layers: 0,
            ..Default::default()
        })
        .expect("open embedder first");
    let reranker = open_reranker(rerank_gguf);

    let rows = embedder.embed_batch(&["panda memo"]).await.expect("embed after reranker open");
    assert_eq!(rows[0].len(), 768);
    let scores = reranker.score_blocking(QUERY, &[RELEVANT]).expect("score after embed");
    assert!(scores[0] > 0.9);
}
