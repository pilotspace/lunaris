//! Episode round-trip integration test against live Postgres + extensions.
//!
//! Gated behind the `pg-it` Cargo feature so CI without a reachable
//! Postgres-with-extensions does NOT fail. To run:
//!
//! ```bash
//! cargo test -p lunaris-storage-postgres --features pg-it --test episode_roundtrip
//! ```
//!
//! RFC 0001 Wave 0: StoragePort methods now take `&Scope`. Tests pass `&Scope::dev()`.

#![cfg(feature = "pg-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, Scope, StorageError, StoragePort, WriteOp};
use lunaris_storage_postgres::PostgresStorage;

fn pg_url() -> String {
    std::env::var("PG_URL").unwrap_or_else(|_| "postgres://localhost/lunaris".into())
}

#[tokio::test]
async fn episode_atomic_write_then_read_back() {
    let s = PostgresStorage::connect(&pg_url())
        .await
        .expect("connect to Postgres — set PG_URL or run Postgres+vector+age+pgmq at localhost");

    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::dev(), "notes.md", "Alice joined Acme on 2024-04-01.", &clock);
    // v0.2.1 keyspace (RFC 0001): `lunaris:{scope}:episode:{ulid}`.
    let key = lunaris_core::keyspace::episode_key(&Scope::dev(), ep.id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    let lsn = s
        .atomic_write(&Scope::dev(), &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write commit");
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "Lsn must be non-zero, got {lsn:?}");

    let now = clock.tick();
    let row = s
        .read_as_of(&Scope::dev(), &key, now)
        .await
        .expect("read_as_of ok")
        .expect("episode exists");
    assert_eq!(row.key, key, "key roundtrip");
    assert_eq!(row.value, Bytes::from(value), "value byte-identical to write");

    let back: Episode = serde_json::from_slice(&row.value).expect("episode deserializes");
    assert_eq!(back.id, ep.id);
    assert_eq!(back.source, ep.source);
    assert_eq!(back.content, ep.content);
}

#[tokio::test]
async fn capabilities_reports_postgres_profile() {
    let s = PostgresStorage::connect(&pg_url()).await.expect("connect");
    let cap = s.capabilities();
    assert!(!cap.bi_temporal_native, "Postgres bi-temporal is emulated");
    assert!(cap.graph_native, "AGE provides graph");
    assert!(!cap.rerank_native);
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 1536);
}

#[tokio::test]
async fn unsupported_url_scheme_rejected() {
    let r = PostgresStorage::connect("mysql://localhost/lunaris").await;
    assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
}

#[cfg(feature = "moon-it")]
#[tokio::test]
async fn cross_backend_identity_with_moon() {
    let moon_url = match std::env::var("MOON_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("SKIP cross_backend_identity_with_moon — set MOON_URL to enable");
            return;
        }
    };
    let pg = PostgresStorage::connect(&pg_url()).await.expect("connect pg");
    let moon = lunaris_storage_moon::MoonStorage::connect(&moon_url).await.expect("connect moon");

    let clock = HlcClock::new(0);
    let ep = Episode::new(Scope::dev(), "notes.md", "cross-backend Alice", &clock);
    // v0.2.1 keyspace (RFC 0001): `lunaris:{scope}:episode:{ulid}`.
    let key = lunaris_core::keyspace::episode_key(&Scope::dev(), ep.id);
    let value = serde_json::to_vec(&ep).unwrap();

    pg.atomic_write(&Scope::dev(), &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .unwrap();
    moon.atomic_write(&Scope::dev(), &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .unwrap();

    let pg_row = pg.read_as_of(&Scope::dev(), &key, clock.tick()).await.unwrap().unwrap();
    let moon_row = moon.read_as_of(&Scope::dev(), &key, clock.tick()).await.unwrap().unwrap();

    assert_eq!(pg_row.value, moon_row.value, "cross-backend value bytes must match");
}
