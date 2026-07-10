//! Weights-gated integration test for [`lunaris_llm::QuantizedCandleBackend`].
//!
//! Gated behind the `candle-quantized` feature. Skipped (not failed) when
//! the real artifacts aren't staged — same convention as
//! `lunaris-rerank-native/tests/quantized_equivalence.rs`.
//!
//! ```bash
//! export LUNARIS_EXTRACTOR_GGUF=/Volumes/Games/tindang-repo/models/gemma-3-4b-it-qat-q4_0/gemma-3-4b-it-q4_0.gguf
//! export LUNARIS_EXTRACTOR_DIR=/Volumes/Games/tindang-repo/models/gemma-3-4b-it-hf
//! cargo test -p lunaris-llm --release --features candle-quantized \
//!     --test quantized_gemma3_decode -- --nocapture
//! ```
//!
//! Reuses the SAME env var names the production ladder
//! (`lunaris_extract::candle_gemma3`) consults, so a dev box configured for
//! the real extractor also drives this test with no extra wiring.
//!
//! ## What this proves
//!
//! - the real QAT Q4_0 GGUF loads via `ModelWeights::from_gguf` without
//!   error (architecture metadata + tensor shapes are self-consistent);
//! - a trivial prompt decodes ≤8 new tokens to non-empty UTF-8 text (the
//!   tokenizer + greedy loop + EOS resolution round-trip correctly);
//! - RSS after load is far below the ~16 GiB F32 baseline (best-effort;
//!   `ps` unavailability degrades to a skipped, non-fatal check — this is
//!   NOT a substitute for the dedicated `extractor_decode` perf bench in
//!   the design doc §5, which measures peak RSS in an isolated process).

#![cfg(feature = "candle-quantized")]

use std::path::PathBuf;
use std::time::Duration;

use candle_core::Device;
use lunaris_llm::{
    GenOpts, LlmBackend, QuantizedCandleBackend, QuantizedCandleBackendOpts, SchemaConstraint,
};

/// Best-effort current-process RSS in KiB via `ps` (macOS/Linux both support
/// `ps -o rss=`). Returns `None` if `ps` is unavailable or its output can't
/// be parsed — callers treat that as "skip this sub-check", not a failure.
fn current_rss_kib() -> Option<u64> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quantized_gemma3_decodes_trivial_prompt() {
    let gguf = match std::env::var_os("LUNARIS_EXTRACTOR_GGUF").map(PathBuf::from) {
        Some(p) if p.exists() => p,
        Some(p) => {
            eprintln!("[skip] LUNARIS_EXTRACTOR_GGUF set but not found at {}", p.display());
            return;
        }
        None => {
            eprintln!(
                "[skip] LUNARIS_EXTRACTOR_GGUF unset; export the path to \
                 gemma-3-4b-it-q4_0.gguf to run this test for real."
            );
            return;
        }
    };
    let dir = match std::env::var_os("LUNARIS_EXTRACTOR_DIR").map(PathBuf::from) {
        Some(p) if p.exists() => p,
        Some(p) => {
            eprintln!("[skip] LUNARIS_EXTRACTOR_DIR set but not found at {}", p.display());
            return;
        }
        None => {
            eprintln!(
                "[skip] LUNARIS_EXTRACTOR_DIR unset; export the dir containing the HF \
                 tokenizer.json + config.json to run this test for real."
            );
            return;
        }
    };
    let tokenizer_path = dir.join("tokenizer.json");
    let config_path = dir.join("config.json");
    if !tokenizer_path.exists() || !config_path.exists() {
        eprintln!(
            "[skip] LUNARIS_EXTRACTOR_DIR={} missing tokenizer.json and/or config.json",
            dir.display()
        );
        return;
    }

    let opts = QuantizedCandleBackendOpts {
        model_name: "gemma-3-4b-it".into(),
        gguf_path: gguf.clone(),
        tokenizer_path,
        config_path,
        device: Device::Cpu,
    };

    eprintln!("[it] loading {} ...", gguf.display());
    let rss_before = current_rss_kib();
    let t0 = std::time::Instant::now();
    let backend =
        QuantizedCandleBackend::new(opts).await.expect("QuantizedCandleBackend::new must succeed");
    let load_ms = t0.elapsed().as_millis();
    let rss_after = current_rss_kib();
    eprintln!("[it] load took {load_ms}ms; model_id = {}", backend.model_id());

    match (rss_before, rss_after) {
        (Some(before), Some(after)) => {
            let delta_mib = after.saturating_sub(before) / 1024;
            eprintln!("[it] RSS delta after load: {delta_mib} MiB");
            // Generous ceiling: Q4 GGUF is ~3 GiB on disk; dequantized norms
            // + embedding table + tokenizer add some overhead, but this
            // must stay WELL under the ~16 GiB F32 baseline (design doc
            // §2). 8 GiB gives headroom without being a vacuous gate.
            assert!(
                delta_mib < 8192,
                "RSS spike too large: {delta_mib} MiB (expected ~3-4 GiB for Q4; ceiling 8 GiB, \
                 well under the 16 GiB F32 baseline this workstream replaces)"
            );
        }
        _ => eprintln!("[it] RSS probe unavailable (ps failed) — skipping RSS sanity check"),
    }

    let t1 = std::time::Instant::now();
    let output = backend
        .generate(
            "The capital of France is",
            SchemaConstraint::None,
            GenOpts { max_tokens: 8, temperature: 0.0, timeout: Duration::from_secs(180) },
        )
        .await
        .expect("generate must succeed");
    let decode_ms = t1.elapsed().as_millis();
    eprintln!("[it] decode took {decode_ms}ms; output = {output:?}");

    assert!(
        !output.trim().is_empty(),
        "decoded output must be non-empty UTF-8 text, got: {output:?}"
    );
}
