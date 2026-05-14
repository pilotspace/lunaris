//! Quantized-equivalence integration test for `NativeQuantizedEmbedder`.
//!
//! **Status: RED.** This test asserts the drift gate that the next-session
//! port must hit. Today the scaffold's `QuantizedModernBert::pooled_forward`
//! returns `NotImplemented`, so the test will fail at the embed step —
//! that's the point of RED-first TDD per CLAUDE.md Rule 3.
//!
//! Gated behind BOTH `embedder-it` and `embedder-gguf`. CI's existing
//! `embedder-it` job picks it up uniformly with:
//!
//! ```bash
//! cargo test -p lunaris-embed-native \
//!     --features 'embedder-it,embedder-gguf' \
//!     --test quantized_equivalence -- --nocapture
//! ```
//!
//! ## Required env vars
//!
//! Skipped (not failed) if any are missing — same convention as the FP16
//! test. The HF cache path comes from the FP16 spawn; the GGUF path comes
//! from `scripts/spike-convert-granite-r2-to-gguf.sh`.
//!
//! - `GRANITE_R2_WEIGHTS_PATH`     — `model.safetensors` (for the FP16 ref path)
//! - `GRANITE_R2_TOKENIZER_PATH`   — `tokenizer.json`
//! - `GRANITE_R2_CONFIG_PATH`      — `config.json`
//! - `GRANITE_R2_GGUF_PATH`        — `granite-r2-311m-Q4_K_M.gguf`
//!
//! ## Drift gates (relaxed vs the FP16 test's f32-epsilon gate)
//!
//! The FP16 test asserts mean ≤ 0.005 / max ≤ 0.02 against the HF reference
//! fixture (because FP16 is bit-equivalent within candle's compute path).
//! Q4_K_M inherently rounds — the gate is relaxed but real:
//!
//! - mean cosine drift ≤ 0.01 (1.0%)  — Q4 vs the FP16 native path
//! - max  cosine drift ≤ 0.03 (3.0%)  — Q4 vs the FP16 native path
//!
//! Comparing Q4 against the FP16 *native* path (not the HF reference
//! fixture) keeps the comparison local to candle's compute graph. The FP16
//! path is already gate-validated against HF, so transitively Q4 is too.
//!
//! ## Non-vacuity guard
//!
//! Cross-prompt dot product < 0.97 — guards against the failure mode where
//! the quantized model degenerates to a constant output that trivially
//! passes the drift gate (because constant-vs-constant cosine = 1).

#![cfg(all(feature = "embedder-it", feature = "embedder-gguf"))]

use std::path::PathBuf;
use std::time::Instant;

use candle_core::Device;
use lunaris_core::Embedder;
use lunaris_embed_native::{
    GRANITE_R2_DIM, GRANITE_R2_GGUF_Q4_SHA256, NativeEmbedder, NativeEmbedderOpts,
    NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts,
};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The exact same 100-prompt panel the FP16 test uses, loaded from the
/// reference-embeddings fixture so both paths see byte-identical inputs.
fn load_panel() -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Fx {
        rows: Vec<Row>,
    }
    #[derive(serde::Deserialize)]
    struct Row {
        prompt: String,
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("reference_embeddings.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let fx: Fx = serde_json::from_slice(&bytes).expect("parse fixture json");
    assert_eq!(fx.rows.len(), 100, "panel must be exactly 100 prompts");
    fx.rows.into_iter().map(|r| r.prompt).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quantized_matches_fp16_native_within_drift_gate() {
    // Skip cleanly if any required artifact is missing.
    let weights = match env_path("GRANITE_R2_WEIGHTS_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] GRANITE_R2_WEIGHTS_PATH unset");
            return;
        }
    };
    let tokenizer = match env_path("GRANITE_R2_TOKENIZER_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] GRANITE_R2_TOKENIZER_PATH unset");
            return;
        }
    };
    let config = match env_path("GRANITE_R2_CONFIG_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] GRANITE_R2_CONFIG_PATH unset");
            return;
        }
    };
    let gguf = match env_path("GRANITE_R2_GGUF_PATH") {
        Some(p) => p,
        None => {
            eprintln!(
                "[skip] GRANITE_R2_GGUF_PATH unset; run \
                 scripts/spike-convert-granite-r2-to-gguf.sh to produce one \
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
    eprintln!("[it] GGUF expected:     {GRANITE_R2_GGUF_Q4_SHA256}");
    eprintln!("[it] GGUF size:         {} MiB", gguf_bytes.len() / 1024 / 1024);
    assert_eq!(
        file_sha, GRANITE_R2_GGUF_Q4_SHA256,
        "GGUF SHA-256 mismatch — re-run scripts/spike-convert-granite-r2-to-gguf.sh"
    );
    drop(gguf_bytes); // don't pin 240 MB while we run inference

    let prompts: Vec<String> = load_panel();
    let prompt_refs: Vec<&str> = prompts.iter().map(|s| s.as_str()).collect();

    // ----- FP16 reference path -------------------------------------------
    let t0 = Instant::now();
    let fp16 = NativeEmbedder::open(NativeEmbedderOpts {
        weights_path: weights,
        tokenizer_path: tokenizer.clone(),
        config_path: config.clone(),
        device: Device::Cpu,
    })
    .expect("FP16 NativeEmbedder::open");
    let fp16_open_ms = t0.elapsed().as_millis();
    assert_eq!(fp16.dim(), GRANITE_R2_DIM);

    let t1 = Instant::now();
    let mut fp16_vecs: Vec<Vec<f32>> = Vec::with_capacity(prompts.len());
    for chunk in prompt_refs.chunks(8) {
        fp16_vecs.extend(fp16.embed_batch(chunk).await.expect("FP16 embed_batch"));
    }
    let fp16_embed_ms = t1.elapsed().as_millis();
    assert_eq!(fp16_vecs.len(), prompts.len());

    // ----- Q4 path -------------------------------------------------------
    let t2 = Instant::now();
    let q4 = NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
        gguf_path: gguf,
        tokenizer_path: tokenizer,
        config_path: config,
        device: Device::Cpu,
    })
    .expect("Q4 NativeQuantizedEmbedder::open");
    let q4_open_ms = t2.elapsed().as_millis();
    assert_eq!(q4.dim(), GRANITE_R2_DIM);

    let t3 = Instant::now();
    let mut q4_vecs: Vec<Vec<f32>> = Vec::with_capacity(prompts.len());
    for chunk in prompt_refs.chunks(8) {
        q4_vecs.extend(q4.embed_batch(chunk).await.expect("Q4 embed_batch"));
    }
    let q4_embed_ms = t3.elapsed().as_millis();
    assert_eq!(q4_vecs.len(), prompts.len());

    // ----- Drift gate ----------------------------------------------------
    let mut drifts: Vec<f64> = Vec::with_capacity(prompts.len());
    let mut max_drift = 0.0_f64;
    let mut max_drift_idx = 0usize;
    for (idx, (a, b)) in fp16_vecs.iter().zip(q4_vecs.iter()).enumerate() {
        assert_eq!(a.len(), GRANITE_R2_DIM);
        assert_eq!(b.len(), GRANITE_R2_DIM);
        let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
        let cos = dot.clamp(-1.0, 1.0);
        let drift = 1.0 - cos;
        if drift > max_drift {
            max_drift = drift;
            max_drift_idx = idx;
        }
        drifts.push(drift);
    }
    let mean: f64 = drifts.iter().sum::<f64>() / (drifts.len() as f64);

    eprintln!("[it] timings — fp16 open {fp16_open_ms}ms / embed {fp16_embed_ms}ms");
    eprintln!("[it] timings — q4   open {q4_open_ms}ms / embed {q4_embed_ms}ms");
    eprintln!(
        "[it] drift Q4 vs FP16-native — mean = {mean:.9}, max = {max_drift:.9} \
         (prompt #{max_drift_idx}: {:?})",
        prompts[max_drift_idx].chars().take(60).collect::<String>()
    );

    // Non-vacuity guard.
    let cross_dot: f64 =
        q4_vecs[0].iter().zip(fp16_vecs[1].iter()).map(|(a, b)| (*a as f64) * (*b as f64)).sum();
    eprintln!("[it] non-vacuity check — cross-prompt dot (q4[0] vs fp16[1]) = {cross_dot:.6}");
    assert!(
        cross_dot.abs() < 0.97,
        "cross-prompt dot {cross_dot:.6} too close to 1.0 — Q4 output may be degenerate"
    );

    // Relaxed drift gate.
    assert!(
        mean <= 0.01,
        "mean cosine drift {mean:.6} exceeds 1.0% gate \
         (Q4_K_M vs FP16-native, 100-prompt panel)"
    );
    assert!(
        max_drift <= 0.03,
        "max cosine drift {max_drift:.6} exceeds 3.0% gate \
         (worst prompt #{max_drift_idx})"
    );
}
