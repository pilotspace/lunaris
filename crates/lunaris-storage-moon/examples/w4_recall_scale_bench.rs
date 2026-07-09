//! W4 validation bench (moon-v051-perf-exploit): strict-replay-style recall
//! latency at 10k-doc scale against live Moon.
//!
//! Philosophy matches the historical strict-replay baseline (p50 10.3 ms /
//! p99 20.8 ms, 10k-doc SQuAD, docs/benchmarks/v0.6-recall-fanout-ab.md):
//! query vectors are precomputed OUTSIDE the timed loop, so the number is
//! retrieval-only. Differences vs that baseline: synthetic clustered vectors
//! instead of SQuAD embeddings, and `StoragePort::vector_search` (KNN k=10 +
//! scope prefilter) instead of the full SDK recall()+hydrate path — so treat
//! it as the substrate floor, not an end-to-end replacement.
//!
//! Run: `MOON_URL=moon://127.0.0.1:7805 cargo run -p lunaris-storage-moon \
//!        --example w4_recall_scale_bench --features moon-it --release`

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::port::MaintenanceHint;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;

const DIM: usize = 768;
const DOCS: usize = 10_000;
const QUERIES: usize = 500;
const K: usize = 10;
const BATCH: usize = 250;

/// Deterministic pseudo-random unit vector around one of 64 cluster centers —
/// clustered data is the shape HNSW/adaptive-ef actually face in production.
fn vec_for(i: usize) -> Vec<f32> {
    let cluster = (i % 64) as f32;
    let mut state = (i as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut v: Vec<f32> = (0..DIM)
        .map(|d| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = ((state >> 33) as f32 / u32::MAX as f32) - 0.5;
            (((d as f32) + cluster * 13.7).sin()) + noise * 0.25
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:7805".into());
    let storage = MoonStorage::connect(&url).await?;
    let scope = Scope::new("w4-bench")?;

    // ── Ingest 10k vector docs (batched atomic writes) ──
    let t0 = std::time::Instant::now();
    for batch_start in (0..DOCS).step_by(BATCH) {
        let ops: Vec<WriteOp> = (batch_start..(batch_start + BATCH).min(DOCS))
            .map(|i| {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&(i as u64).to_be_bytes());
                id[8..].copy_from_slice(&(i as u64).to_le_bytes());
                WriteOp::VectorUpsert {
                    index: "chunks".into(),
                    id: id.to_vec(),
                    embedding: vec_for(i),
                    metadata: serde_json::json!({
                        "text": format!("synthetic doc {i}"),
                        "source": format!("bench/{}", i % 10),
                    }),
                }
            })
            .collect();
        storage.atomic_write(&scope, &ops).await?;
    }
    let ingest_s = t0.elapsed().as_secs_f64();
    println!("ingest: {DOCS} docs in {ingest_s:.1}s ({:.0} docs/s)", DOCS as f64 / ingest_s);

    // ── Compact (the W1 maintenance path) so search hits HNSW + exact rerank ──
    let t1 = std::time::Instant::now();
    storage
        .maintenance_hint(&scope, MaintenanceHint::BulkIngestComplete { vector_upserts: DOCS })
        .await?;
    println!("compact: {:.2}s", t1.elapsed().as_secs_f64());

    // ── Precompute query vectors OUT of the timed loop (strict replay) ──
    let queries: Vec<Vec<f32>> = (0..QUERIES).map(|q| vec_for(q * 17 + 3)).collect();

    // Warm-up (connection + first-search lazy costs out of the measurement).
    for q in queries.iter().take(20) {
        let _ = storage.vector_search(&scope, "chunks", q, K, None, None, false).await?;
    }

    let mut lat_us: Vec<u64> = Vec::with_capacity(QUERIES);
    let mut total_hits = 0usize;
    for q in &queries {
        let t = std::time::Instant::now();
        let hits = storage.vector_search(&scope, "chunks", q, K, None, None, false).await?;
        lat_us.push(t.elapsed().as_micros() as u64);
        total_hits += hits.len();
    }
    lat_us.sort_unstable();
    let p = |f: f64| lat_us[((lat_us.len() as f64 * f) as usize).min(lat_us.len() - 1)];
    println!(
        "recall k={K} over {DOCS} docs: p50={:.2}ms p90={:.2}ms p99={:.2}ms (n={QUERIES}, avg_hits={:.1})",
        p(0.50) as f64 / 1000.0,
        p(0.90) as f64 / 1000.0,
        p(0.99) as f64 / 1000.0,
        total_hits as f64 / QUERIES as f64,
    );
    Ok(())
}
