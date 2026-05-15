//! O-01-F — per-device embedder + reranker p50/p99 benches against the
//! hardware-target gate table from
//! `.planning/milestones/v0.4-NATIVE-SINGLE-BACKEND/HARDWARE-OPTIMIZATION-ROADMAP.md`
//! §26-37.
//!
//! Gates (measured at batch=8, K=10 for rerank):
//!
//! | Host                          | Embed p50 | Embed p99 | Rerank p50 |
//! |-------------------------------|-----------|-----------|------------|
//! | Apple Silicon (Metal)         | ≤ 5 ms    | ≤ 15 ms   | ≤ 40 ms    |
//! | NVIDIA sm_80+ (CUDA)          | ≤ 3 ms    | ≤ 10 ms   | ≤ 25 ms    |
//! | x86_64 CPU + Accelerate/MKL   | ≤ 40 ms   | ≤ 90 ms   | ≤ 300 ms   |
//! | aarch64 CPU + NEON            | ≤ 80 ms   | ≤ 200 ms  | ≤ 600 ms   |
//!
//! ## Backend selection
//!
//! Run with `cargo bench --features <backend> --bench per_device`. The
//! [`select_bench_device`] helper uses the same `Device::Cpu`-default-with-
//! feature-upgrade rule as `lunaris-embed-native::device_select`: pass
//! `Device::Cpu` and the bench will upgrade to Metal / CUDA when the
//! matching feature is on.
//!
//! ## Weights resolution
//!
//! Reads paths from the env vars (mirroring the integration-test convention):
//!   - `GRANITE_R2_WEIGHTS_PATH`  / `GRANITE_R2_TOKENIZER_PATH`  / `GRANITE_R2_CONFIG_PATH`
//!   - `BGE_RERANKER_WEIGHTS_PATH` / `BGE_RERANKER_TOKENIZER_PATH` / `BGE_RERANKER_CONFIG_PATH`
//!
//! When any required env var is unset the matching backend / model is
//! SKIPPED with an `eprintln!` notice (mirrors the `helios_p50.rs` /
//! `consolidator_tick.rs` skip discipline so the bench is safe to run on
//! dev boxes that don't have weights on disk).
//!
//! **O-01 host scope**: this commit lands the bench harness. Real CPU +
//! Metal measurements are produced in O-03 self-hosted runners with weights
//! cached; the placeholder baselines doc at
//! `docs/benchmarks/o01/per-device-baselines.md` documents the gate values
//! against the measurement procedure.

use std::path::PathBuf;
use std::time::Duration;

use candle_core::Device;
use criterion::{Criterion, criterion_group, criterion_main};

/// Resolve a `(weights, tokenizer, config)` triple from env vars, returning
/// `None` (and printing a skip notice) if any are missing.
fn weights_triple(prefix: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let w = std::env::var(format!("{prefix}_WEIGHTS_PATH")).ok()?;
    let t = std::env::var(format!("{prefix}_TOKENIZER_PATH")).ok()?;
    let c = std::env::var(format!("{prefix}_CONFIG_PATH")).ok()?;
    Some((PathBuf::from(w), PathBuf::from(t), PathBuf::from(c)))
}

/// Backend label that drives the Criterion bench group name (becomes part
/// of the `target/criterion/<group>/<bench>` path so per-backend baselines
/// don't collide).
fn backend_label() -> &'static str {
    #[cfg(feature = "cuda-fa2")]
    {
        return "cuda-fa2";
    }
    #[cfg(all(feature = "cuda", not(feature = "cuda-fa2")))]
    {
        return "cuda";
    }
    #[cfg(feature = "metal")]
    {
        return "metal";
    }
    #[cfg(feature = "cpu-mkl")]
    {
        return "cpu-mkl";
    }
    #[cfg(feature = "cpu-accelerate")]
    {
        return "cpu-accelerate";
    }
    #[cfg(not(any(
        feature = "cuda-fa2",
        feature = "cuda",
        feature = "metal",
        feature = "cpu-mkl",
        feature = "cpu-accelerate"
    )))]
    {
        "cpu-default"
    }
}

fn embed_bench(c: &mut Criterion) {
    let Some((w, t, cfg)) = weights_triple("GRANITE_R2") else {
        eprintln!(
            "skip: granite-r2 weights env vars unset \
             (GRANITE_R2_{{WEIGHTS,TOKENIZER,CONFIG}}_PATH)"
        );
        return;
    };
    let opts = lunaris_embed_native::NativeEmbedderOpts {
        weights_path: w,
        tokenizer_path: t,
        config_path: cfg,
        device: Device::Cpu, // upgraded by device_select if `metal`/`cuda` is on
    };
    let embedder = match lunaris_embed_native::NativeEmbedder::open(opts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skip: NativeEmbedder::open failed: {e}");
            return;
        }
    };

    // The 8-prompt panel exercises the public-API batch ceiling
    // (`MAX_PUBLIC_BATCH = 8`) end-to-end — a single chunk.
    let prompts: Vec<&str> = vec![
        "Lunaris is an agent memory engine.",
        "Bộ nhớ tác tử là cốt lõi của Lunaris.",
        "fn main() { println!(\"hello\"); }",
        "ModernBERT alternates local + global attention.",
        "Mean cosine drift must stay below the 0.5% gate.",
        "Apple Silicon Metal backend uses unified memory.",
        "Re-chunking the public batch caps activation OOM.",
        "Granite-r2 emits 768-dimensional unit vectors.",
    ];

    let label = backend_label();
    let mut group = c.benchmark_group(format!("embed/{label}"));
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(50);
    group.bench_function("batch_8", |b| {
        b.iter(|| {
            embedder.embed_blocking(&prompts).expect("embed_blocking failed");
        });
    });
    group.finish();
}

fn rerank_bench(c: &mut Criterion) {
    let Some((w, t, cfg)) = weights_triple("BGE_RERANKER") else {
        eprintln!(
            "skip: bge-reranker weights env vars unset \
             (BGE_RERANKER_{{WEIGHTS,TOKENIZER,CONFIG}}_PATH)"
        );
        return;
    };
    let opts = lunaris_rerank_native::NativeRerankerOpts {
        weights_path: w,
        tokenizer_path: t,
        config_path: cfg,
        device: Device::Cpu,
    };
    let reranker = match lunaris_rerank_native::NativeReranker::open(opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip: NativeReranker::open failed: {e}");
            return;
        }
    };

    // K=10 matches the rerank gate's test point. With MAX_PUBLIC_BATCH=8
    // this is two chunks (8 + 2) through `score_blocking` directly. The
    // bench measures the sync path so chunking variance from `tokio::spawn`
    // scheduling doesn't pollute the perf signal.
    let query = "What does Lunaris store?";
    let docs: Vec<(&str, &str)> = (0..10)
        .map(|i| {
            (
                query,
                match i % 3 {
                    0 => "Lunaris stores bi-temporal facts with MVCC.",
                    1 => "Lunaris is a Rust framework with Python and TS SDKs.",
                    _ => "Lunaris exposes a composable retrieval DSL.",
                },
            )
        })
        .collect();

    let label = backend_label();
    let mut group = c.benchmark_group(format!("rerank/{label}"));
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(50);
    group.bench_function("k10", |b| {
        b.iter(|| {
            reranker.score_blocking(&docs).expect("score_blocking failed");
        });
    });
    group.finish();
}

criterion_group!(per_device_benches, embed_bench, rerank_bench);
criterion_main!(per_device_benches);
