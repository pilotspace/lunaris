//! Phase D (llama.cpp-only cutover) — Criterion bench for the llama.cpp
//! embed/rerank hot path. Replaces the deleted candle `per_device` bench
//! (O-01-F): the device × quant matrix collapsed to one runtime, so the
//! gate is now three steady-state cells on whatever device the build
//! targets (CPU on GitHub-hosted runners; `--features metal` locally).
//!
//! Cells:
//!  - `embed/batch8_short` — 8 one-sentence texts (recall-query shape)
//!  - `embed/batch8_long`  — 8 ~120-word paragraphs (ingest-chunk shape)
//!  - `rerank/1x8`         — 1 query × 8 candidates (RETRIEVE-06 shape)
//!
//! ## Skip discipline (mirrors recall_hot_path)
//!
//! Missing GGUF artifacts → print a skip banner and register no benches.
//! CI stages the artifacts first (perf-gates.yml downloads + SHA-verifies
//! them), so a skip there is loud: perf-gate-check fails on an empty
//! criterion dir only when a baseline exists, and the workflow asserts the
//! artifacts staged before invoking the bench.
//!
//! Regression gating is relative (Criterion saved baseline + the
//! `perf-gate-check` bin, 5% cliff) — absolute-time budgets are unstable
//! on shared runners.

use std::path::PathBuf;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use lunaris_core::Embedder;
use lunaris_rerank::{RerankCandidate, Reranker};

const EMBEDDER_GGUF_ENV: &str = "LUNARIS_EMBEDDER_GGUF";
const RERANKER_GGUF_ENV: &str = "LUNARIS_RERANKER_GGUF";

fn staged_default(file: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lunaris")
        .join("models")
        .join(file)
}

fn resolve_gguf(env_var: &str, staged_name: &str) -> Option<PathBuf> {
    let path = std::env::var(env_var)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| staged_default(staged_name));
    path.is_file().then_some(path)
}

fn short_texts() -> Vec<String> {
    (0..8)
        .map(|i| format!("Alice prefers dark chocolate over milk chocolate, sample {i}."))
        .collect()
}

fn long_texts() -> Vec<String> {
    let para = "The consolidation supervisor batches working-memory events by scope, \
        deduplicates against the fact store via blake3 entity identity, and emits \
        one atomic write per ingest tick. Retrieval fuses vector, keyword, and \
        graph lanes with reciprocal-rank fusion before the cross-encoder pass. ";
    (0..8).map(|i| format!("{} Variant {i}.", para.repeat(4))).collect()
}

fn rerank_candidates() -> Vec<RerankCandidate> {
    short_texts()
        .into_iter()
        .enumerate()
        .map(|(i, text)| RerankCandidate {
            id: vec![i as u8; 16],
            text,
            score: 0.0,
            metadata: serde_json::Value::Null,
        })
        .collect()
}

fn bench_llamacpp(c: &mut Criterion) {
    let embedder_gguf =
        resolve_gguf(EMBEDDER_GGUF_ENV, "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf");
    let reranker_gguf = resolve_gguf(RERANKER_GGUF_ENV, "bge-reranker-v2-m3.Q5_K_M.gguf");

    let rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");

    match embedder_gguf {
        None => eprintln!(
            "llamacpp_hot_path: SKIP embed cells — no GGUF at ${EMBEDDER_GGUF_ENV} or the \
             ~/.lunaris/models/ staged default"
        ),
        Some(gguf_path) => {
            let opts = lunaris_llamacpp::LlamaCppEmbedderOpts { gguf_path, ..Default::default() };
            let embedder: Arc<dyn Embedder> = Arc::new(
                lunaris_llamacpp::LlamaCppEmbedder::open(opts).expect("open staged embedder GGUF"),
            );
            // One throwaway call so the first measured iteration reuses the
            // warm context (EncodeWorker creates it lazily).
            let warm = short_texts();
            let warm_refs: Vec<&str> = warm.iter().map(String::as_str).collect();
            rt.block_on(embedder.embed_batch(&warm_refs)).expect("warmup embed");

            let mut group = c.benchmark_group("embed");
            group.sample_size(20);
            for (cell, texts) in [("batch8_short", short_texts()), ("batch8_long", long_texts())] {
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                group.bench_function(cell, |b| {
                    b.iter(|| {
                        let rows = rt.block_on(embedder.embed_batch(&refs)).expect("embed");
                        assert_eq!(rows.len(), refs.len());
                    });
                });
            }
            group.finish();
        }
    }

    match reranker_gguf {
        None => eprintln!(
            "llamacpp_hot_path: SKIP rerank cell — no GGUF at ${RERANKER_GGUF_ENV} or the \
             ~/.lunaris/models/ staged default"
        ),
        Some(gguf_path) => {
            let opts = lunaris_llamacpp::LlamaCppRerankerOpts { gguf_path, ..Default::default() };
            let reranker =
                lunaris_llamacpp::LlamaCppReranker::open(opts).expect("open staged reranker GGUF");
            rt.block_on(reranker.rerank("warmup query", rerank_candidates()))
                .expect("warmup rerank");

            let mut group = c.benchmark_group("rerank");
            group.sample_size(20);
            group.bench_function("1x8", |b| {
                b.iter(|| {
                    let out = rt
                        .block_on(
                            reranker
                                .rerank("what chocolate does Alice prefer?", rerank_candidates()),
                        )
                        .expect("rerank");
                    assert_eq!(out.len(), 8);
                });
            });
            group.finish();
        }
    }
}

criterion_group!(benches, bench_llamacpp);
criterion_main!(benches);
