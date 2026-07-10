//! Phase A2 acceptance — one long-lived llama.cpp context per model,
//! reused across calls (the "context pool", cutover unknown #2).
//!
//! The §4b/ADR finding: the spike created a context per `embed_blocking`
//! call, and on Metal that per-call setup dominated (1,266 tok/s observed
//! vs llama-bench's 13,650 tok/s warm ceiling for the same GGUF). A2 moves
//! model + ONE context into a dedicated worker thread (no self-referential
//! struct, everything freed on drop) so repeat calls hit a warm context.
//!
//! The discriminating observable is `contexts_created()`: context-per-call
//! reads N after N calls; the pooled design reads exactly 1. Output
//! contracts must be unchanged (dims, normalization, score discrimination).

#![cfg(feature = "llamacpp")]

use std::path::PathBuf;

use lunaris_llamacpp::{
    LlamaCppEmbedder, LlamaCppEmbedderOpts, LlamaCppReranker, LlamaCppRerankerOpts,
};

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

fn reranker_gguf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_RERANKER_GGUF").map(PathBuf::from) {
        return Some(p);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf"))
        .filter(|p| p.exists())
}

#[test]
fn embedder_reuses_one_context_across_calls() {
    let Some(gguf_path) = embedder_gguf() else {
        eprintln!("[skip] embedder_reuses_one_context_across_calls — granite GGUF not staged");
        return;
    };
    let embedder = LlamaCppEmbedder::open(LlamaCppEmbedderOpts {
        gguf_path,
        n_gpu_layers: if cfg!(feature = "metal") { u32::MAX } else { 0 },
        ..Default::default()
    })
    .expect("open");

    let mut first: Option<Vec<f32>> = None;
    for _ in 0..3 {
        let rows = embedder.embed_blocking(&["a tiny memo", "another memo"]).expect("embed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 768);
        // Warm-context determinism: the same input through the SAME context
        // must reproduce bitwise (no per-call context state).
        match &first {
            None => first = Some(rows[0].clone()),
            Some(f) => assert_eq!(&rows[0], f, "same context must reproduce identical rows"),
        }
    }
    assert_eq!(
        embedder.contexts_created(),
        1,
        "3 calls must reuse ONE warm context — context-per-call is the A2 regression"
    );
}

#[test]
fn reranker_reuses_one_context_across_calls() {
    let Some(gguf_path) = reranker_gguf() else {
        eprintln!("[skip] reranker_reuses_one_context_across_calls — bge GGUF not staged");
        return;
    };
    let reranker = LlamaCppReranker::open(LlamaCppRerankerOpts {
        gguf_path,
        n_gpu_layers: if cfg!(feature = "metal") { u32::MAX } else { 0 },
        ..Default::default()
    })
    .expect("open");

    for _ in 0..3 {
        let scores = reranker
            .score_blocking(
                "what is panda?",
                &["hi", "The giant panda is a bear species endemic to China."],
            )
            .expect("score");
        assert!(scores[1] > 0.9 && scores[0] < 0.1, "discrimination contract must survive A2");
    }
    assert_eq!(
        reranker.contexts_created(),
        1,
        "3 calls must reuse ONE warm context — context-per-call is the A2 regression"
    );
}
