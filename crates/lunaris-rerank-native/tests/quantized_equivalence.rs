//! Quantized-equivalence integration test for `NativeQuantizedReranker`.
//!
//! **Status: RED at first landing.** The scaffold's `QuantizedXlmRoberta::load`
//! returns `NotImplemented`, so the test fails at the `open` step until the
//! forward-pass commit lands. That's the point of RED-first TDD per
//! CLAUDE.md Rule 3.
//!
//! Gated behind BOTH `reranker-it` and `reranker-gguf`. CI's existing
//! `reranker-it` job picks it up uniformly with:
//!
//! ```bash
//! cargo test -p lunaris-rerank-native \
//!     --features 'reranker-it,reranker-gguf' --release \
//!     --test quantized_equivalence -- --nocapture
//! ```
//!
//! ## Required env vars
//!
//! Skipped (not failed) if any are missing — same convention as the FP32
//! `numerical_equivalence.rs` test.
//!
//! - `BGE_RERANKER_WEIGHTS_PATH`   — `model.safetensors` (for the FP32 ref path)
//! - `BGE_RERANKER_TOKENIZER_PATH` — `tokenizer.json`
//! - `BGE_RERANKER_CONFIG_PATH`    — `config.json`
//! - `BGE_RERANKER_GGUF_PATH`      — `bge-reranker-v2-m3-Q4_K_M.gguf`
//!
//! ## Drift gate
//!
//! Comparing the Q4 sigmoid-bounded score against the FP32 native path
//! sigmoid-bounded score on the same `(query, doc)` pair:
//!
//! - **max absolute score delta ≤ 0.10** per pair (per the N-02 step 2
//!   brief — the load-bearing gate)
//! - **mean absolute score delta ≤ 0.04** (derived: sigmoid is monotonic
//!   + smooth; 0.10 in sigmoid space corresponds to ~0.4 in logit space at
//!   logit≈0. A mean drift this loose would imply systemic shift; the
//!   gate catches it.)
//!
//! Why compare against the FP32 native path (not the HF python reference)?
//! - keeps the comparison local to candle's compute graph;
//! - the FP32 native path is independently gate-validated against HF in
//!   `tests/numerical_equivalence.rs`;
//! - transitively the Q4 path is validated against HF too.
//!
//! ## Non-vacuity guard
//!
//! Native Q4 score variance > 0.01 — guards against the failure mode where
//! the quantized model degenerates to a constant output that trivially
//! passes the drift gate (because constant-vs-constant abs(delta) = 0).

#![cfg(all(feature = "reranker-it", feature = "reranker-gguf"))]

use std::path::PathBuf;
use std::time::Instant;

use candle_core::Device;
use lunaris_rerank_native::{
    BGE_RERANKER_GGUF_Q4_SHA256, NativeQuantizedReranker, NativeQuantizedRerankerOpts,
    NativeReranker, NativeRerankerOpts,
};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// Load the (query, doc) pair list from the schema fixture. We deliberately
/// ignore `reference_score` here — this test compares Q4 vs the FP32 native
/// path, both run inside the test process, so the python reference is not
/// in the loop.
fn load_pairs() -> Vec<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Fx {
        pairs: Vec<Pair>,
    }
    #[derive(serde::Deserialize)]
    struct Pair {
        query: String,
        doc: String,
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("reference_scores.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let fx: Fx = serde_json::from_slice(&bytes).expect("parse fixture json");
    assert_eq!(fx.pairs.len(), 100, "panel must be exactly 100 pairs");
    fx.pairs.into_iter().map(|p| (p.query, p.doc)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quantized_matches_fp32_native_within_drift_gate() {
    // Skip cleanly if any required artifact is missing.
    let weights = match env_path("BGE_RERANKER_WEIGHTS_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] BGE_RERANKER_WEIGHTS_PATH unset");
            return;
        }
    };
    let tokenizer = match env_path("BGE_RERANKER_TOKENIZER_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] BGE_RERANKER_TOKENIZER_PATH unset");
            return;
        }
    };
    let config = match env_path("BGE_RERANKER_CONFIG_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] BGE_RERANKER_CONFIG_PATH unset");
            return;
        }
    };
    let gguf = match env_path("BGE_RERANKER_GGUF_PATH") {
        Some(p) => p,
        None => {
            eprintln!(
                "[skip] BGE_RERANKER_GGUF_PATH unset; run \
                 scripts/spike-convert-bge-reranker-to-gguf.sh to produce one \
                 and export the path."
            );
            return;
        }
    };

    // SHA-256 sanity check — refuse to run against an arbitrary GGUF.
    let gguf_bytes = std::fs::read(&gguf).expect("read gguf");
    let file_sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&gguf_bytes);
        let out = h.finalize();
        out.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    eprintln!("[it] GGUF SHA-256:      {file_sha}");
    eprintln!("[it] GGUF expected:     {BGE_RERANKER_GGUF_Q4_SHA256}");
    eprintln!("[it] GGUF size:         {} MiB", gguf_bytes.len() / 1024 / 1024);
    assert_eq!(
        file_sha, BGE_RERANKER_GGUF_Q4_SHA256,
        "GGUF SHA-256 mismatch — re-run scripts/spike-convert-bge-reranker-to-gguf.sh"
    );
    drop(gguf_bytes); // don't pin 418 MB during inference

    let pairs: Vec<(String, String)> = load_pairs();
    let pair_refs: Vec<(&str, &str)> =
        pairs.iter().map(|(q, d)| (q.as_str(), d.as_str())).collect();

    // ----- FP32 reference path -------------------------------------------
    let t0 = Instant::now();
    let fp32 = NativeReranker::open(NativeRerankerOpts {
        weights_path: weights,
        tokenizer_path: tokenizer.clone(),
        config_path: config.clone(),
        device: Device::Cpu,
    })
    .expect("FP32 NativeReranker::open");
    let fp32_open_ms = t0.elapsed().as_millis();

    let t1 = Instant::now();
    let mut fp32_scores: Vec<f32> = Vec::with_capacity(pairs.len());
    for chunk in pair_refs.chunks(8) {
        let s = fp32.score_blocking(chunk).expect("FP32 score_blocking");
        fp32_scores.extend(s);
    }
    let fp32_score_ms = t1.elapsed().as_millis();
    assert_eq!(fp32_scores.len(), pairs.len());

    // ----- Q4 path -------------------------------------------------------
    let t2 = Instant::now();
    let q4 = NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
        gguf_path: gguf,
        tokenizer_path: tokenizer,
        config_path: config,
        device: Device::Cpu,
    })
    .expect("Q4 NativeQuantizedReranker::open");
    let q4_open_ms = t2.elapsed().as_millis();

    let t3 = Instant::now();
    let mut q4_scores: Vec<f32> = Vec::with_capacity(pairs.len());
    for chunk in pair_refs.chunks(8) {
        let s = q4.score_blocking(chunk).expect("Q4 score_blocking");
        q4_scores.extend(s);
    }
    let q4_score_ms = t3.elapsed().as_millis();
    assert_eq!(q4_scores.len(), pairs.len());

    // ----- Drift gate ----------------------------------------------------
    let mut deltas: Vec<f64> = Vec::with_capacity(pairs.len());
    let mut max_delta = 0.0_f64;
    let mut max_delta_idx = 0usize;
    for (idx, (a, b)) in fp32_scores.iter().zip(q4_scores.iter()).enumerate() {
        let d = ((*a as f64) - (*b as f64)).abs();
        if d > max_delta {
            max_delta = d;
            max_delta_idx = idx;
        }
        deltas.push(d);
    }
    let mean_delta: f64 = deltas.iter().sum::<f64>() / (deltas.len() as f64);

    // Variance of Q4 scores (non-vacuity).
    let q4_mean: f64 =
        q4_scores.iter().map(|s| *s as f64).sum::<f64>() / (q4_scores.len() as f64);
    let q4_var: f64 = q4_scores.iter().map(|s| (*s as f64 - q4_mean).powi(2)).sum::<f64>()
        / (q4_scores.len() as f64);
    let fp32_mean: f64 =
        fp32_scores.iter().map(|s| *s as f64).sum::<f64>() / (fp32_scores.len() as f64);
    let fp32_var: f64 = fp32_scores.iter().map(|s| (*s as f64 - fp32_mean).powi(2)).sum::<f64>()
        / (fp32_scores.len() as f64);

    eprintln!("[it] timings — fp32 open {fp32_open_ms}ms / score {fp32_score_ms}ms");
    eprintln!("[it] timings — q4   open {q4_open_ms}ms / score {q4_score_ms}ms");
    eprintln!(
        "[it] score delta — mean = {mean_delta:.6}, max = {max_delta:.6} \
         (pair #{max_delta_idx}: q={:?}, d={:?})",
        pairs[max_delta_idx].0.chars().take(40).collect::<String>(),
        pairs[max_delta_idx].1.chars().take(40).collect::<String>()
    );
    eprintln!(
        "[it] variance — fp32 = {fp32_var:.6}, q4 = {q4_var:.6} \
         (means: fp32 {fp32_mean:.4}, q4 {q4_mean:.4})"
    );

    // Non-vacuity guards.
    assert!(
        fp32_var > 0.01,
        "FP32 variance {fp32_var:.6} is too low — reference is degenerate, \
         drift gate would be vacuous"
    );
    assert!(
        q4_var > 0.01,
        "Q4 variance {q4_var:.6} is too low — model collapsed to a near-constant \
         output, drift gate would be vacuous"
    );

    // Load-bearing gate from the N-02 step 2 brief.
    assert!(
        max_delta <= 0.10,
        "max abs score delta {max_delta:.6} exceeds 0.10 gate \
         (worst pair #{max_delta_idx})"
    );
    assert!(
        mean_delta <= 0.04,
        "mean abs score delta {mean_delta:.6} exceeds 0.04 derived gate \
         (sigmoid-space; tighter than max to catch systemic shift)"
    );
}
