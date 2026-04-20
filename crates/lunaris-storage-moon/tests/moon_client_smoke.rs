//! Phase 1.5 retrofit (STORE-09) smoke test — proves that after swapping the
//! hand-rolled `redis 0.32+` RESP wrappers for the typed `moon-client` v0.1.x SDK,
//! the `MoonStorage` round-trip and capability surface still hold.
//!
//! Gated behind the `moon-it` Cargo feature so CI without a reachable Moon instance
//! does NOT fail on the per-commit gate. To run:
//!
//! ```bash
//! cargo test -p lunaris-storage-moon --features moon-it --test moon_client_smoke
//! ```
//!
//! Expects Moon at `MOON_URL` env var (default `moon://localhost:6390`).
//!
//! ## What this proves
//!
//! 1. `MoonStorage::connect(url)` now uses `moon-client::MoonClient::connect(...)`
//!    under the hood and produces an Episode round-trip identical to Phase 1's
//!    `episode_roundtrip.rs`.
//! 2. `MoonStorage::capabilities().native_rrf == true` after the retrofit, which
//!    is the contract Phase 2's `fuse_rrf` operator depends on when picking
//!    `RrfFusion::Moon` over `RrfFusion::Client`.
//!
//! ## What this does NOT prove
//!
//! End-to-end `text().hybrid_search()` against live indexes — that is exercised in
//! Phase 2 plan 02-02 (Keyword BM25 + RRF) once the BM25 + vector indexes exist.
//! Here we only assert the `native_rrf` capability bit so the planner has the
//! signal it needs.

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, StoragePort, WriteOp};
use lunaris_storage_moon::{MoonStorage, keyspace};

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6390".into())
}

#[tokio::test]
async fn round_trip_via_moon_client() {
    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6390");

    let clock = HlcClock::new(0);
    let ep = Episode::new("smoke://retrofit", "hello moon-client", &clock);
    let key = keyspace::episode_key(ep.id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    let lsn = storage
        .atomic_write(&[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write commit via moon-client");
    assert!(
        lsn.wall_ms > 0 || lsn.counter > 0,
        "Lsn must be non-zero after the retrofit, got {lsn:?}"
    );

    let now = clock.tick();
    let row = storage
        .read_as_of(&key, now)
        .await
        .expect("read_as_of ok via moon-client")
        .expect("episode exists");
    assert_eq!(row.key, key, "key roundtrip via moon-client");
    assert_eq!(
        row.value,
        Bytes::from(value),
        "value roundtrip — bytes must be identical via moon-client"
    );
}

#[tokio::test]
async fn capabilities_reports_native_rrf() {
    let storage = MoonStorage::connect(&moon_url()).await.expect("connect to Moon");
    let cap = storage.capabilities();
    assert!(
        cap.native_rrf,
        "Moon backend MUST report native_rrf=true after Phase 1.5 retrofit \
         so Phase 2's fuse_rrf operator can pick RrfFusion::Moon"
    );
    // Also re-assert the rest of the Moon profile so any drift surfaces here.
    assert!(cap.bi_temporal_native);
    assert!(cap.graph_native);
    assert!(cap.rerank_native);
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768);
}
