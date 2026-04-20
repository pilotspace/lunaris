//! Plan 02-04 — Criterion bench for the Phase 1 storage hot path.
//!
//! Measures `StoragePort::atomic_write` ALONE — single chunk-shaped batch
//! (1 KvPut + 1 VectorUpsert with 768-d random vector). Isolates the storage
//! layer from chunker + embedder overhead so the blueprint §4.1 atomic-write
//! row (3 ms p50 / 12 ms p99 on Moon; ~8 ms p50 on Postgres native floor)
//! has a clean measurement.
//!
//! Why bench atomic_write separately from ingest? The ingest bench measures
//! the full pipeline (chunk → embed → atomic_write). When the ingest p50
//! regresses, this bench tells us whether the regression is in the storage
//! layer OR in the pre-storage stages.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lunaris::Lunaris;
use lunaris_bench::tcp_reachable;
use lunaris_core::WriteOp;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;

struct Backend {
    label: &'static str,
    env: &'static str,
}

const BACKENDS: &[Backend] =
    &[Backend { label: "moon", env: "MOON_URL" }, Backend { label: "postgres", env: "PG_URL" }];

const EMBEDDING_DIM: usize = 768;

fn atomic_write_benches(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("atomic_write_hot_path");
    // atomic_write is the hottest of the three benches (3-12ms range), so we
    // can do many samples in a tight wall clock.
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(15));
    group.warm_up_time(Duration::from_secs(3));

    for backend in BACKENDS {
        let Ok(url) = std::env::var(backend.env) else {
            eprintln!("SKIP atomic_write/{} bench: {} unset", backend.label, backend.env);
            continue;
        };
        if !runtime.block_on(tcp_reachable(&url)) {
            eprintln!(
                "SKIP atomic_write/{} bench: backend unreachable (1s TCP probe failed)",
                backend.label
            );
            continue;
        }

        let handle = match runtime.block_on(Lunaris::open(&url)) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("SKIP atomic_write/{} bench: open failed: {}", backend.label, e);
                continue;
            }
        };
        let storage = handle.storage();

        // Pre-build the fixed WriteOp shape ONCE outside the iter loop so
        // we measure storage round-trip, not Vec / serde construction. The
        // VectorUpsert id is randomised per-iteration via a counter (below)
        // so we don't cheat with key-overwrite-locality.
        let embedding: Vec<f32> = {
            let mut rng = ChaCha20Rng::seed_from_u64(7);
            (0..EMBEDDING_DIM).map(|_| rng.random_range(-1.0_f32..1.0_f32)).collect()
        };

        group.bench_with_input(
            BenchmarkId::new("atomic_write", backend.label),
            &embedding.len(),
            |b, _len| {
                let storage = storage.clone();
                let embedding = embedding.clone();
                b.to_async(&runtime).iter(|| {
                    let storage = storage.clone();
                    let embedding = embedding.clone();
                    async move {
                        // Random id per iteration so we measure insert, not
                        // overwrite. ulid::Ulid::new() is monotonic and
                        // ~100 ns; negligible vs the storage round-trip.
                        let id = ulid::Ulid::new();
                        let key = format!("lunaris:bench:{id}").into_bytes();
                        let ops = vec![
                            WriteOp::KvPut { key: key.clone(), value: b"bench-payload".to_vec() },
                            WriteOp::VectorUpsert {
                                index: "facts".to_string(),
                                id: id.to_bytes().to_vec(),
                                embedding,
                                metadata: serde_json::json!({"bench": true}),
                            },
                        ];
                        let _lsn = storage.atomic_write(&ops).await.expect("atomic_write");
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, atomic_write_benches);
criterion_main!(benches);
