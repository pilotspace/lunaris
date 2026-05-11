//! `FastembedEmbedder` integration test matrix — Phase 19-02.
//!
//! Gated on **both** `fastembed` and `embedder-it` features so the default
//! `cargo test -p lunaris-embed` run never compiles or executes this file
//! (the `--test fastembed_it` binary itself is conditionally empty when the
//! features are off).
//!
//! ## Local-only caveat
//!
//! Tests 1–3 perform a **real ONNX forward pass** of EmbeddingGemma 300M via
//! `fastembed::TextEmbedding`. First run auto-downloads ~600 MB of weights
//! from Hugging Face Hub (30–90s cold). Subsequent runs hit the cache at
//! `$LUNARIS_FASTEMBED_CACHE_DIR` / `~/.cache/lunaris/models/fastembed/`.
//! CI integration is deferred — see `19-02-SUMMARY.md` "CI gap".
//!
//! Test 4 is **offline-safe**: it points fastembed at a path that cannot be a
//! directory (`/dev/null/...`), so `try_new` fails on cache-dir creation
//! **before** any network call.
//!
//! ## Scenarios
//!
//! 1. `batch_of_four_returns_four_normalised_rows` — batch-axis correctness +
//!    L2 unit norm + 768-d width across all rows.
//! 2. `identical_inputs_produce_identical_vectors` — exact f32 equality for
//!    two identical strings embedded in **one** call (catches stateful
//!    mutation in the `&mut self` adapter).
//! 3. `two_separate_calls_produce_stable_output` — exact f32 equality across
//!    **two** `embed_batch` invocations on the same embedder (catches
//!    Mutex-induced state leakage or per-call seed bugs).
//! 4. `bogus_cache_dir_returns_actionable_error` — construct with a cache
//!    path under `/dev/null` (a char device — `mkdir` under it is impossible
//!    on macOS/Linux). Expect `LunarisError::Storage(StorageError::Backend(_))`
//!    with an `fastembed` prefix.

#![cfg(all(feature = "fastembed", feature = "embedder-it"))]

use std::path::PathBuf;

use lunaris_core::{Embedder, LunarisError, StorageError};
use lunaris_embed::{FastembedEmbedder, FastembedEmbedderOpts};
use lunaris_embed::fastembed::FASTEMBED_GEMMA_DIM;

/// Build a fresh embedder using the default cache resolution chain. Callers
/// can pre-warm by setting `LUNARIS_FASTEMBED_CACHE_DIR` to a pre-populated
/// directory before running these tests offline.
fn fresh_embedder() -> FastembedEmbedder {
    FastembedEmbedder::new(FastembedEmbedderOpts::default())
        .expect("FastembedEmbedder::new — set LUNARIS_FASTEMBED_CACHE_DIR if running offline")
}

/// Tolerance for L2 norm assertions. ONNX runtime + host-side f64
/// normalisation should land within 1e-3 of unit; tighter tolerances
/// trip on cross-platform fp differences.
const L2_TOL: f64 = 1e-3;

/// Scenario 1 — batch-axis correctness.
///
/// Catches: collapsed-batch bugs (e.g., returning a single row for N inputs),
/// silently truncated outputs, mis-shaped rows (≠ 768d), or graph emissions
/// that bypass our defensive L2-normalise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_four_returns_four_normalised_rows() {
    let embedder = fresh_embedder();
    assert_eq!(embedder.dim(), FASTEMBED_GEMMA_DIM);

    let inputs: Vec<&str> = vec!["alpha", "beta", "gamma", "delta"];
    let vecs = embedder
        .embed_batch(&inputs)
        .await
        .expect("embed_batch (batch of 4)");

    assert_eq!(vecs.len(), 4, "expected 4 rows, got {}", vecs.len());
    for (idx, row) in vecs.iter().enumerate() {
        assert_eq!(
            row.len(),
            FASTEMBED_GEMMA_DIM,
            "row {idx} dim = {}, expected {FASTEMBED_GEMMA_DIM}",
            row.len()
        );
        let l2 = row.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        assert!(
            (l2 - 1.0).abs() < L2_TOL,
            "row {idx} L2 = {l2}, expected within {L2_TOL} of 1.0"
        );
    }
}

/// Scenario 2 — single-batch determinism.
///
/// Two identical strings in the **same** call must produce element-wise
/// identical f32 rows. ONNX is deterministic on CPU for a fixed batch, so
/// any drift here implies the `&mut self`→`&self` adapter is leaking state
/// across the batch axis.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_inputs_produce_identical_vectors() {
    let embedder = fresh_embedder();
    let inputs: Vec<&str> = vec!["lunaris memory engine", "lunaris memory engine"];
    let vecs = embedder
        .embed_batch(&inputs)
        .await
        .expect("embed_batch (identical pair)");

    assert_eq!(vecs.len(), 2);
    assert_eq!(vecs[0].len(), FASTEMBED_GEMMA_DIM);
    assert_eq!(vecs[1].len(), FASTEMBED_GEMMA_DIM);
    assert_eq!(
        vecs[0], vecs[1],
        "identical inputs in same batch produced divergent f32 rows — adapter state leak"
    );
}

/// Scenario 3 — cross-call stability.
///
/// Same embedder instance, same input, two separate `embed_batch` calls.
/// Result must be byte-identical. Catches: Mutex-induced state leakage
/// inside `TextEmbedding`, accidental RNG seeds, or any per-call mutation
/// of the ORT session that affects subsequent inference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_separate_calls_produce_stable_output() {
    let embedder = fresh_embedder();
    let inputs: Vec<&str> = vec!["alpha"];

    let first = embedder
        .embed_batch(&inputs)
        .await
        .expect("embed_batch (call 1)");
    let second = embedder
        .embed_batch(&inputs)
        .await
        .expect("embed_batch (call 2)");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(
        first[0], second[0],
        "two calls to embed_batch with identical input produced divergent rows"
    );
}

/// Scenario 4 — cache-dir error mapping (offline-safe).
///
/// Strategy: point fastembed at a path under `/dev/null`. On macOS and
/// Linux, `/dev/null` is a character device — `mkdir /dev/null/anything`
/// is `ENOTDIR`. fastembed's `with_cache_dir` triggers a `create_dir_all`
/// inside `try_new`, which fails immediately. That keeps the test
/// **offline-safe** (no HF Hub round-trip even on a cold cache), portable
/// across Unix, and catches regressions in the `anyhow_to_lunaris` mapping
/// that surfaces `fastembed: ...` to operators.
///
/// Windows note: `\\.\NUL` is the rough equivalent but its `mkdir` semantics
/// differ; this test is `cfg(unix)` to avoid that landmine. The lunaris-embed
/// `fastembed` feature is presently Unix-only in practice anyway (ONNX
/// EmbeddingGemma weights tested on macOS + Linux).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bogus_cache_dir_returns_actionable_error() {
    let bogus = PathBuf::from("/dev/null/lunaris-fastembed-it/cannot-exist");
    let opts = FastembedEmbedderOpts {
        cache_dir: Some(bogus),
        show_download_progress: false,
    };
    let err = FastembedEmbedder::new(opts)
        .expect_err("constructing under /dev/null/... must fail before any network call");

    match err {
        LunarisError::Storage(StorageError::Backend(msg)) => {
            // fastembed reported: <message>
            // The literal message comes from fastembed's `anyhow::Error`
            // chain (typically "Failed to create cache directory" wrapped
            // by `ort` / `hf-hub`). Capture it for the SUMMARY.
            assert!(
                msg.starts_with("fastembed"),
                "expected actionable `fastembed: ...` prefix from anyhow_to_lunaris, got: {msg}"
            );
            eprintln!("// fastembed reported: {msg}");
        }
        other => panic!(
            "expected LunarisError::Storage(StorageError::Backend(_)), got: {other:?}"
        ),
    }
}
