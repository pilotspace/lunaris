//! Plan 02-04 — Criterion bench for the Phase 2 ingest hot path.
//!
//! Measures `Lunaris::ingest(Episode { 12 KB markdown })` end-to-end on the
//! default recipe (no graph). The blueprint §4.1 latency budget asserts
//! p50 ≤ 50 ms / p99 ≤ 110 ms on both MoonStorage AND PostgresStorage; this
//! bench provides the measurements that `tests/budget_assertions.rs` then
//! enforces.
//!
//! ## Backend selection
//!
//! Each backend opts in via an env var:
//! - `MOON_URL=moon://localhost:6390` → registers the `ingest_12kb_md/moon` bench.
//! - `PG_URL=postgres://lunaris:lunaris@localhost/lunaris` → registers
//!   `ingest_12kb_md/postgres`.
//!
//! When a URL env is unset OR the host:port doesn't accept a 1-sec TCP probe,
//! the bench group emits `eprintln!("SKIP …")` and continues without
//! registering that backend. Empty bench groups (both backends unreachable)
//! finalise with zero registered benches → Criterion reports "no benches".
//!
//! ## Embedder + reranker choices
//!
//! Default: `lunaris_core::StubEmbedder::new(768)` and `NoopReranker`. These
//! isolate the storage + DSL hot path from the EmbeddingGemma forward pass
//! cost (~8ms p50 per blueprint §4.2) so a regression in storage shows up
//! cleanly. The "with embedder" / "with rerank" variants are deferred to a
//! future plan that adds them behind `LUNARIS_EMBED_GEMMA_PATH` /
//! `LUNARIS_RERANK_BGE_PATH` env gates (Plan 02-04 ships the storage-hot-path
//! measurement first; embedder + reranker cold-start measurement is the
//! Plan 02-04 follow-up tracked in the SUMMARY).
//!
//! ## Sample size + warmup
//!
//! Default Criterion `sample_size = 100` × ~50 ms/sample = 5 s per backend
//! per bench. We override to `sample_size = 30` and `measurement_time = 30 s`
//! so each backend completes in ~30 s (faster CI feedback; still tight CI on
//! p99 with 30 samples per the central limit theorem).

use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lunaris::Lunaris;
use lunaris_bench::{tcp_reachable, twelve_kb_markdown_fixture};
use lunaris_core::{Episode, StubEmbedder};

/// Bench-local backend descriptor. Pairs a human label with the env-var URL
/// so the SKIP messages render the label, not the URL (T-02-04-01).
struct Backend {
    label: &'static str,
    env: &'static str,
}

const BACKENDS: &[Backend] =
    &[Backend { label: "moon", env: "MOON_URL" }, Backend { label: "postgres", env: "PG_URL" }];

fn ingest_benches(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("ingest_hot_path");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(30));
    group.warm_up_time(Duration::from_secs(5));

    let fixture = twelve_kb_markdown_fixture();

    let mut registered_any = false;
    for backend in BACKENDS {
        let Ok(url) = std::env::var(backend.env) else {
            eprintln!("SKIP ingest/{} bench: {} unset", backend.label, backend.env);
            continue;
        };
        // 1-sec TCP probe so an unreachable backend doesn't burn 30s per
        // sample on the open() retry path.
        if !runtime.block_on(tcp_reachable(&url)) {
            eprintln!(
                "SKIP ingest/{} bench: backend unreachable (1s TCP probe failed)",
                backend.label
            );
            continue;
        }
        // Build the handle ONCE per backend; the inner iter loop reuses it.
        // StubEmbedder isolates the storage hot path from the EmbeddingGemma
        // forward-pass cost.
        let handle = match runtime.block_on(async {
            let h = Lunaris::open(&url).await?;
            Ok::<_, lunaris::LunarisError>(h.with_embedder(Arc::new(StubEmbedder::new(768))))
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("SKIP ingest/{} bench: open failed: {}", backend.label, e);
                continue;
            }
        };

        registered_any = true;
        group.bench_with_input(
            BenchmarkId::new("ingest_12kb_md", backend.label),
            &fixture,
            |b, content| {
                b.to_async(&runtime).iter(|| async {
                    // Each iteration constructs a fresh Episode (new ULID) so
                    // we measure ingest, not idempotent re-write.
                    let ep = Episode::new("bench/12kb_doc.md", *content, &handle.clock());
                    let _lsn = handle.ingest(ep).await.expect("ingest");
                });
            },
        );
    }

    if !registered_any {
        eprintln!(
            "ingest_hot_path: no live backends reachable; skipping group. \
             Set MOON_URL and/or PG_URL to populate Criterion data."
        );
    }
    group.finish();
}

criterion_group!(benches, ingest_benches);
criterion_main!(benches);
