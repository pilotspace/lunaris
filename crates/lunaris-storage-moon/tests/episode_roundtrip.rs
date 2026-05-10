//! Episode round-trip integration test against live Moon.
//!
//! Gated behind the `moon-it` Cargo feature so CI without a reachable Moon instance does
//! NOT fail on the per-commit gate. To run:
//!
//! ```bash
//! cargo test -p lunaris-storage-moon --features moon-it --test episode_roundtrip
//! ```
//!
//! Expects Moon at `MOON_URL` env var (default `moon://localhost:6390`).
//!
//! RFC 0001 Wave 0: StoragePort methods now take `&Scope`. Tests pass `&Scope::dev()`.

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, Scope, StorageError, StoragePort, WriteOp};
use lunaris_storage_moon::{MoonStorage, keyspace};

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6390".into())
}

#[tokio::test]
async fn episode_atomic_write_then_read_back() {
    let s = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6390");

    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::dev(), "notes.md", "Alice joined Acme on 2024-04-01.", &clock);
    let key = keyspace::episode_key(ep.id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    // Single KvPut to keep the round-trip simple.
    let lsn = s
        .atomic_write(&Scope::dev(), &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write commit");
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "Lsn must be non-zero, got {lsn:?}",);

    // Read via read_as_of(now()) — must return exactly the bytes we wrote.
    let now = clock.tick();
    let row = s
        .read_as_of(&Scope::dev(), &key, now)
        .await
        .expect("read_as_of ok")
        .expect("episode exists");
    assert_eq!(row.key, key, "key roundtrip");
    assert_eq!(row.value, Bytes::from(value), "value roundtrip — bytes must be identical");
}

/// Capabilities are independent of any live Moon state — but if we connect we should
/// always see the Moon native-everything profile.
#[tokio::test]
async fn capabilities_reports_moon_native_profile() {
    let s = MoonStorage::connect(&moon_url()).await.expect("connect to Moon");
    let cap = s.capabilities();
    assert!(cap.bi_temporal_native);
    assert!(cap.graph_native);
    assert!(cap.rerank_native);
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768);
}

/// Defense-in-depth check: even with the moon-it feature, a non-`moon://` URL is rejected
/// before any TCP IO.
#[tokio::test]
async fn unsupported_url_scheme_rejected() {
    let r = MoonStorage::connect("redis://localhost:6379").await;
    assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
}
