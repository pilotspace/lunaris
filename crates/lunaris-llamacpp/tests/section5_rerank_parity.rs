//! §5 rerank drift regression baseline (llama.cpp-only cutover) — the
//! llama.cpp Q5_K_M reranker vs PINNED candle-FP32 reference scores on the
//! 100-pair panel (`tests/fixtures/section5_panel.json`).
//!
//! The reference scores were captured 2026-07-10 (pre-Phase-C, while the
//! candle FP32 `NativeReranker` still existed) from the path that was
//! itself HF-gate-validated by the retired candle reranker's tests/
//! numerical_equivalence.rs` (see git history) — so passing here
//! transitively tracks HF FP32.
//!
//! Thresholds (owner decision 2026-07-10, superseding the design-doc §5
//! targets): this is a REGRESSION BASELINE, not an acceptance gate — the
//! load-bearing quality bar for the cutover was the end-to-end LongMemEval
//! run (measured J = 96.0%, recall@10 = 98.0% with this exact Q5 stack, vs
//! candle baseline 94.0%/96.0%). The original §5 targets (p95 ≤ 0.02,
//! inversions ≤ 1%) were design-doc aspirations no shipped quantized path
//! ever met — the incumbent candle-Q5 measured p95 0.047 / inv 2.12% /
//! max 0.185 on this panel; runtime-only drift (same GGUF, llamacpp vs
//! candle) was p95 0.021 / inv 1.25%. Thresholds below = measured llamacpp
//! values + margin so a kernel/dequant regression still fails loudly:
//! - |Δscore| p95 ≤ 0.08 (measured 0.063);
//! - pairwise order-inversion ≤ 3% across C(100,2) (measured 2.32%);
//! - non-vacuity: Q5 score variance > 0.01 (a constant output trivially
//!   passes both drift bounds).
//!
//! Skips (does not fail) when the Q5 GGUF is not staged — same convention
//! as every other artifact-dependent test.

#![cfg(feature = "llamacpp")]

use std::path::PathBuf;

use lunaris_llamacpp::{LlamaCppReranker, LlamaCppRerankerOpts};

fn q5_gguf() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_RERANKER_GGUF").map(PathBuf::from) {
        return Some(p);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf"))
        .filter(|p| p.exists())
}

/// The 100-pair panel with pinned FP32 reference scores.
fn load_panel() -> Vec<(String, String, f32)> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/section5_panel.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("parse fixture json");
    let pairs: Vec<(String, String, f32)> = v["pairs"]
        .as_array()
        .expect("pairs array")
        .iter()
        .map(|p| {
            (
                p["query"].as_str().expect("query").to_string(),
                p["doc"].as_str().expect("doc").to_string(),
                p["fp32_score"].as_f64().expect("fp32_score pinned") as f32,
            )
        })
        .collect();
    assert_eq!(pairs.len(), 100, "panel must be exactly 100 pairs");
    pairs
}

/// 2026-07-10 attribution measurement (this host, CPU):
/// - llamacpp-Q5 vs FP32:  p95 = 0.063, max = 0.091, inversions = 2.32%
/// - candle-Q5  vs FP32:   p95 = 0.047, max = 0.185, inversions = 2.12%
/// - llamacpp-Q5 vs candle-Q5 (same GGUF): p95 = 0.021, inversions = 1.25%
///
/// Runtime-only drift is small; the bulk is quantization itself, shared
/// with the deleted candle incumbent. llamacpp-Q5 PASSES the previously
/// shipped candle-Q4 gate (max ≤ 0.10) with max 0.091 and a better worst
/// case than candle-Q5 had.
#[test]
fn llamacpp_q5_stays_within_measured_drift_baseline_vs_pinned_fp32() {
    let Some(gguf) = q5_gguf() else {
        eprintln!("[skip] section5 baseline — bge Q5_K_M GGUF not staged");
        return;
    };
    let panel = load_panel();
    let fp32_scores: Vec<f32> = panel.iter().map(|(_, _, s)| *s).collect();

    // llama.cpp Q5 path — per-pair queries, one warm context.
    let q5 = LlamaCppReranker::open(LlamaCppRerankerOpts {
        gguf_path: gguf,
        n_gpu_layers: 0, // CPU: baseline numbers must not depend on the build GPU
        ..Default::default()
    })
    .expect("open LlamaCppReranker");
    let mut q5_scores: Vec<f32> = Vec::with_capacity(panel.len());
    for (q, d, _) in &panel {
        let s = q5.score_blocking(q, &[d.as_str()]).expect("Q5 score_blocking");
        q5_scores.push(s[0]);
    }

    // ── Bound 1: |Δscore| p95 ≤ 0.08 (sigmoid space) ─────────────────────
    let mut deltas: Vec<f32> =
        fp32_scores.iter().zip(&q5_scores).map(|(a, b)| (a - b).abs()).collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let p95 = deltas[(0.95 * (deltas.len() as f64 - 1.0)).round() as usize];
    let max = *deltas.last().expect("non-empty");

    // ── Bound 2: pairwise order-inversion ≤ 3% ───────────────────────────
    let n = panel.len();
    let mut inversions = 0usize;
    let mut comparisons = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            let ref_d = fp32_scores[i] - fp32_scores[j];
            let q5_d = q5_scores[i] - q5_scores[j];
            comparisons += 1;
            if ref_d * q5_d < 0.0 {
                inversions += 1;
            }
        }
    }
    let inversion_rate = inversions as f64 / comparisons as f64;

    // ── Bound 3: non-vacuity ─────────────────────────────────────────────
    let mean = q5_scores.iter().sum::<f32>() / n as f32;
    let variance = q5_scores.iter().map(|s| (s - mean) * (s - mean)).sum::<f32>() / n as f32;

    println!(
        "[section5] |Δ| p95 = {p95:.5}  max = {max:.5}  inversions = {inversions}/{comparisons} \
         ({:.3}%)  q5 variance = {variance:.4}",
        inversion_rate * 100.0
    );

    assert!(variance > 0.01, "Q5 output degenerated to near-constant (variance {variance:.5})");
    assert!(
        p95 <= 0.08,
        "|Δscore| p95 {p95:.5} exceeds the regression baseline of 0.08 (measured 0.063 at \
         cutover — a kernel/dequant change likely regressed)"
    );
    assert!(
        inversion_rate <= 0.03,
        "order-inversion rate {:.3}% exceeds the regression baseline of 3% (measured 2.32% at \
         cutover) ({inversions}/{comparisons})",
        inversion_rate * 100.0
    );
}
