//! Spike acceptance test for the llama.cpp embedder (ADR
//! `docs/decisions/2026-07-10-llamacpp-inference-runtime.md` §"Spike
//! acceptance").
//!
//! Gated behind `--features llamacpp` (builds llama.cpp via cmake) and the
//! staged granite-r2 Q4_K_M GGUF; skips (does not fail) when the artifact
//! is missing — same convention as the native crates' gated tests.
//!
//! Asserts the output contract (dim=768, L2-normalized, out[i]↔inputs[i],
//! non-degenerate cross-prompt cosine < 0.97 — the same non-vacuity guard
//! as `quantized_equivalence.rs`) and prints forward tokens/s so the run
//! can be compared against the §4b candle numbers for the same host.

#![cfg(feature = "llamacpp")]

use std::path::PathBuf;
use std::time::Instant;

use lunaris_core::Embedder;
use lunaris_llamacpp::{LlamaCppEmbedder, LlamaCppEmbedderOpts};

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // Rows are L2-normalized by the embedder; the dot product IS the cosine.
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn gguf_path() -> Option<PathBuf> {
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

#[tokio::test]
async fn llamacpp_embedder_smoke_and_throughput() {
    let Some(gguf_path) = gguf_path() else {
        eprintln!(
            "[skip] llamacpp_embedder_smoke_and_throughput — granite-r2 GGUF not staged \
             (set LUNARIS_EMBEDDER_GGUF)"
        );
        return;
    };

    let embedder = LlamaCppEmbedder::open(LlamaCppEmbedderOpts {
        gguf_path,
        // CPU by default so the test runs anywhere the feature builds; pass
        // n_gpu_layers=u32::MAX + `--features metal` for the Metal number.
        n_gpu_layers: if cfg!(feature = "metal") { u32::MAX } else { 0 },
        ..Default::default()
    })
    .expect("open llama.cpp embedder");
    assert_eq!(embedder.dim(), 768);

    let long = "the quarterly architecture review covered the storage engine migration, \
                the bi-temporal fact compaction schedule, and the retrieval fusion weights. "
        .repeat(8);
    let inputs =
        vec!["a tiny memo about the standup.", long.as_str(), "a tiny memo about the standup."];

    let start = Instant::now();
    let rows = embedder.embed_batch(&inputs).await.expect("embed_batch");
    let wall = start.elapsed();

    // Contract: one 768-d L2-normalized row per input, positionally aligned.
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.len(), 768);
        let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "rows must be L2-normalized, got norm {norm}");
    }
    // Spike observation (documented, not a bug): llama.cpp packs ragged
    // sequences into one ubatch, and identical inputs at different pack
    // positions come back with ~1e-3-level element differences (accumulation
    // order + Q4 group boundaries shift with position) — unlike candle's
    // uniformly-padded batches where identical rows are bitwise-identical.
    // Gate on cosine instead; the quality gates in plan §5 (order-inversion,
    // |Δscore| p95) are the real arbiter before any default flips.
    let same = cosine(&rows[0], &rows[2]);
    assert!(same > 0.999, "identical inputs must embed near-identically, cosine {same}");

    // Non-vacuity: distinct prompts must not collapse to one embedding.
    let cross = cosine(&rows[0], &rows[1]);
    assert!(cross < 0.97, "cross-prompt cosine {cross} suggests a degenerate constant output");

    // Rough throughput print for comparison against §4b (not asserted — CI
    // hosts vary; the number is the point, the gate is the contract above).
    // Two passes: the first encode pays backend graph compile (cold); the
    // second hits the A2 warm context — the number that compares against
    // llama-bench's warm ceiling.
    let approx_tokens = inputs.iter().map(|s| s.len() / 4).sum::<usize>();
    let backend = if cfg!(feature = "metal") { "metal" } else { "cpu" };
    println!(
        "llamacpp embed (cold): ~{approx_tokens} tokens in {wall:?} (~{:.0} tok/s, {backend})",
        approx_tokens as f64 / wall.as_secs_f64(),
    );

    let start = Instant::now();
    let rows_warm = embedder.embed_batch(&inputs).await.expect("warm embed_batch");
    let warm = start.elapsed();
    assert_eq!(rows_warm.len(), 3);
    println!(
        "llamacpp embed (warm): ~{approx_tokens} tokens in {warm:?} (~{:.0} tok/s, {backend})",
        approx_tokens as f64 / warm.as_secs_f64(),
    );
}
