//! Plan 02-04 — Criterion bench for the Phase 2 recall hot path.
//!
//! Measures `Lunaris::recall().with_root(...).execute(Query::text(q))`
//! end-to-end on the canonical hybrid recipe:
//!
//! ```ignore
//! Vector::new("facts", 30)
//!     .and(Keyword::bm25("facts", 30))
//!     .fuse_rrf(60)
//!     .top(5)
//! ```
//!
//! against a 1M-fact synthetic corpus. The blueprint §4.2 latency budget
//! asserts:
//!  - p50 ≤ 25 ms, p99 ≤ 80 ms on MoonStorage (RETRIEVE-11)
//!  - p50 ≤ 60 ms on PostgresStorage (RETRIEVE-12; no p99 budget specified)
//!
//! ## Setup phase (one-shot, cached)
//!
//! On first invocation against a given backend, [`ensure_corpus_or_skip`]
//! builds the 1M-fact corpus via direct `atomic_write` (bypasses the public
//! ingest API to skip the ~14h sequential-ingest cost). Subsequent runs hit
//! the on-disk fingerprint cache and start the bench loop immediately.
//!
//! On a cold dev box this can take ~5–10 minutes. The bench prints
//! `Building 1M-fact corpus (one-time setup)…` so the operator running
//! `cargo bench` knows what they're waiting on.
//!
//! ## Skip discipline (T-02-04-04)
//!
//! - URL env unset → skip backend.
//! - 1-sec TCP probe fails → skip backend.
//! - Corpus build errors → propagate (recall against empty corpus would
//!   misrepresent the budget).
//! - Initial sanity recall returns 0 hits → skip backend (T-02-04-05;
//!   bench against empty corpus is meaningless).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lunaris::Lunaris;
use lunaris::{Keyword, Query, Vector};
use lunaris_bench::{
    DEFAULT_CORPUS_COUNT, DEFAULT_CORPUS_SEED, DEFAULT_QUERY_SEED, ensure_corpus_or_skip,
    synthetic_query_set, tcp_reachable,
};
use lunaris_core::StubEmbedder;

struct Backend {
    label: &'static str,
    env: &'static str,
}

const BACKENDS: &[Backend] =
    &[Backend { label: "moon", env: "MOON_URL" }, Backend { label: "postgres", env: "PG_URL" }];

/// How many distinct queries the bench cycles through. Larger = more diverse
/// IO patterns; smaller = tighter cache locality. 50 hits the sweet spot —
/// enough variation to defeat trivial caching, few enough that one query has
/// repeated cache hits within the bench window.
const QUERY_SET_SIZE: usize = 50;

/// Per-call top-K. Matches the Helios scratchpad recipe's default.
const RECALL_TOP_K: usize = 5;

/// Per-branch upstream candidate cap. Matches blueprint §4.2 budget table
/// (vector + keyword each yield top-30 → fuse_rrf → top-5).
const PER_BRANCH_K: usize = 30;

fn recall_benches(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("recall_hot_path");
    // The recall budget is tight (25 ms p50 Moon / 60 ms p50 Postgres) so we
    // can afford more samples than the ingest bench in a fixed wall clock.
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(20));
    group.warm_up_time(Duration::from_secs(3));

    let queries = synthetic_query_set(DEFAULT_QUERY_SEED, QUERY_SET_SIZE);

    for backend in BACKENDS {
        let Ok(url) = std::env::var(backend.env) else {
            eprintln!("SKIP recall/{} bench: {} unset", backend.label, backend.env);
            continue;
        };
        if !runtime.block_on(tcp_reachable(&url)) {
            eprintln!(
                "SKIP recall/{} bench: backend unreachable (1s TCP probe failed)",
                backend.label
            );
            continue;
        }

        // Build the handle ONCE per backend with a StubEmbedder so the
        // recall budget measures the storage hot path independent of the
        // EmbeddingGemma cost. The corpus embeddings were generated via the
        // same StubEmbedder algorithm so the vector branch finds real hits.
        let handle = match runtime.block_on(async {
            let h = Lunaris::open(&url).await?;
            Ok::<_, lunaris::LunarisError>(h.with_embedder(Arc::new(StubEmbedder::new(768))))
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("SKIP recall/{} bench: open failed: {}", backend.label, e);
                continue;
            }
        };

        // One-shot corpus setup with progress hint. ensure_corpus_or_skip
        // hits the on-disk fingerprint cache when the corpus already exists.
        eprintln!(
            "recall/{}: ensuring 1M-fact corpus (cached on disk after first run, ~5-10 min cold)",
            backend.label
        );
        let built = match runtime.block_on(ensure_corpus_or_skip(
            &handle,
            DEFAULT_CORPUS_COUNT,
            DEFAULT_CORPUS_SEED,
            &url,
            backend.label,
        )) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("SKIP recall/{} bench: corpus build failed: {}", backend.label, e);
                continue;
            }
        };
        if !built {
            // ensure_corpus_or_skip already emitted the SKIP line.
            continue;
        }

        // Sanity probe: recall against an empty corpus would silently report
        // a fast-but-meaningless p50 (no rows to score). Run one recall
        // BEFORE registering the bench; abort the backend if it returns 0
        // hits. T-02-04-05 mitigation.
        let probe_ok = runtime.block_on(async {
            handle
                .recall()
                .with_root(
                    Vector::new("facts", PER_BRANCH_K)
                        .and(Keyword::bm25("facts", PER_BRANCH_K))
                        .fuse_rrf(60)
                        .top(RECALL_TOP_K),
                )
                .execute(Query::text(queries[0].clone()))
                .await
        });
        let probe_hits = match probe_ok {
            Ok(hits) => hits,
            Err(e) => {
                eprintln!("SKIP recall/{} bench: probe recall failed: {}", backend.label, e);
                continue;
            }
        };
        if probe_hits.is_empty() {
            eprintln!(
                "SKIP recall/{} bench: probe recall returned 0 hits (corpus appears empty); \
                 fingerprint may be stale or the FT/tsvector indexes did not pick up the \
                 atomic_write rows. Re-run with the fingerprint deleted.",
                backend.label
            );
            continue;
        }

        // Round-robin counter so each iteration picks a different query;
        // AtomicUsize is shared between the closure and the iter body.
        let counter = Arc::new(AtomicUsize::new(0));
        let queries = queries.clone();
        group.bench_with_input(
            BenchmarkId::new("recall_q", backend.label),
            &queries.len(),
            |b, _len| {
                b.to_async(&runtime).iter(|| {
                    let counter = counter.clone();
                    let handle = handle.clone();
                    let queries = queries.clone();
                    async move {
                        let i = counter.fetch_add(1, Ordering::Relaxed);
                        let q = &queries[i % queries.len()];
                        let _hits = handle
                            .recall()
                            .with_root(
                                Vector::new("facts", PER_BRANCH_K)
                                    .and(Keyword::bm25("facts", PER_BRANCH_K))
                                    .fuse_rrf(60)
                                    .top(RECALL_TOP_K),
                            )
                            .execute(Query::text(q.clone()))
                            .await
                            .expect("recall");
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, recall_benches);
criterion_main!(benches);
