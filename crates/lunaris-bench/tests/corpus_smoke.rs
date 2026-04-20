//! Plan 02-04 — corpus generator smoke test.
//!
//! Builds a 100-fact corpus (NOT 1M) against an in-memory `RecordingStorage`
//! and verifies the bulk-write path works end-to-end. The recall benches in
//! `benches/recall_hot_path.rs` call the same `build_one_million_fact_corpus`
//! function with `count = 1_000_000` against a live Moon / Postgres backend;
//! this test proves the call shape compiles + runs + commits the right ops.
//!
//! Why 100 not 1M? CI must finish the workspace test suite in seconds, not
//! minutes. The 1M variant is exercised by the live-backend bench harness
//! (which a developer runs manually with MOON_URL set).

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_bench::corpus::tests_recording::RecordingStorage;
use lunaris_bench::{
    CORPUS_EMBEDDING_DIM, CorpusFingerprint, build_one_million_fact_corpus, fingerprint_path_for,
    hash_url,
};
use lunaris_core::{Embedder, HlcClock, StoragePort, StubEmbedder};

const SMOKE_COUNT: u64 = 100;
const SMOKE_SEED: u64 = 42;
const FAKE_URL: &str = "moon://lunaris-bench-smoke:9999";

#[tokio::test]
async fn small_corpus_builds_against_recording_storage() {
    // Clean any prior smoke fingerprint so the build actually runs.
    let fp = fingerprint_path_for(FAKE_URL, SMOKE_COUNT);
    let _ = std::fs::remove_file(&fp);

    let recorder = Arc::new(RecordingStorage::default());
    let storage: Arc<dyn StoragePort> = recorder.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(CORPUS_EMBEDDING_DIM));
    let handle = Lunaris::with_parts(storage, embedder, HlcClock::new(0));

    build_one_million_fact_corpus(&handle, SMOKE_COUNT, SMOKE_SEED, FAKE_URL)
        .await
        .expect("smoke corpus build");

    // Two ops per fact (KvPut + VectorUpsert) → 200 ops total.
    let (kv_puts, vec_upserts) = recorder.op_kind_counts();
    assert_eq!(kv_puts, SMOKE_COUNT as usize, "100 facts → 100 KvPut ops");
    assert_eq!(vec_upserts, SMOKE_COUNT as usize, "100 facts → 100 VectorUpsert ops");
    assert_eq!(recorder.total_ops(), (SMOKE_COUNT * 2) as usize);

    // Fingerprint must exist + record completion.
    let bytes = std::fs::read(&fp).expect("fingerprint file");
    let parsed: CorpusFingerprint = serde_json::from_slice(&bytes).expect("fingerprint parse");
    assert!(parsed.completed);
    assert_eq!(parsed.fact_count, SMOKE_COUNT);
    assert_eq!(parsed.seed, SMOKE_SEED);
    assert_eq!(parsed.backend_url_hash, hash_url(FAKE_URL));
    assert!(parsed.duration_secs > 0.0);
}

#[tokio::test]
async fn second_call_with_same_seed_hits_fingerprint_cache() {
    // Pre-write a fingerprint for a fresh fake URL (avoid contamination from
    // the other test that uses a different URL).
    let cache_url = "moon://lunaris-bench-cachehit:9999";
    let fp = fingerprint_path_for(cache_url, SMOKE_COUNT);
    let _ = std::fs::remove_file(&fp);

    let recorder = Arc::new(RecordingStorage::default());
    let storage: Arc<dyn StoragePort> = recorder.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(CORPUS_EMBEDDING_DIM));
    let handle = Lunaris::with_parts(storage, embedder, HlcClock::new(0));

    // First build populates storage + writes fingerprint.
    build_one_million_fact_corpus(&handle, SMOKE_COUNT, SMOKE_SEED, cache_url)
        .await
        .expect("first build");
    let first_total = recorder.total_ops();
    assert_eq!(first_total, (SMOKE_COUNT * 2) as usize);

    // Second build with same seed/count/url MUST cache-hit and NOT call
    // atomic_write a second time.
    build_one_million_fact_corpus(&handle, SMOKE_COUNT, SMOKE_SEED, cache_url)
        .await
        .expect("second build (cache hit)");
    let second_total = recorder.total_ops();
    assert_eq!(
        second_total, first_total,
        "second invocation with matching fingerprint must skip rebuild; got {first_total} → {second_total}"
    );
}

#[tokio::test]
async fn changing_seed_invalidates_cache() {
    let url = "moon://lunaris-bench-seedinvalidate:9999";
    let fp = fingerprint_path_for(url, SMOKE_COUNT);
    let _ = std::fs::remove_file(&fp);

    let recorder = Arc::new(RecordingStorage::default());
    let storage: Arc<dyn StoragePort> = recorder.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(CORPUS_EMBEDDING_DIM));
    let handle = Lunaris::with_parts(storage, embedder, HlcClock::new(0));

    build_one_million_fact_corpus(&handle, SMOKE_COUNT, 1, url).await.expect("seed=1 build");
    let after_first = recorder.total_ops();
    build_one_million_fact_corpus(&handle, SMOKE_COUNT, 2, url).await.expect("seed=2 build");
    let after_second = recorder.total_ops();
    assert!(
        after_second > after_first,
        "seed change must rebuild (after_first={after_first}, after_second={after_second})"
    );
}
