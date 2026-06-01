//! URL-scheme routing tests for `lunaris::open(url)`.
//!
//! ## Test split (after Plan 04)
//!
//! Plan 02 wired both `MoonStorage::connect` and `PostgresStorage::connect` to
//! URL-only stubs that always succeeded; Plans 03 / 04 replaced them with real
//! `redis::aio::ConnectionManager::new(...)` and `sqlx::postgres::PgPool::connect(...)`
//! respectively. Tests that actually open a Moon / Postgres URL now depend on a running
//! backend at that URL — they are gated behind the `moon-it` and `pg-it` features.
//!
//! Tests for invalid URL schemes (rediss / garbage) still run in CI without any backend,
//! because the URL allowlist rejects them BEFORE any backend constructor dials out.

// `lunaris::*` is only needed when the moon-it / pg-it gates are on; without them the
// only thing the test file uses is `lunaris::open(...)`. We reference it explicitly to
// keep the import surface honest under the default feature set.
#[allow(unused_imports)]
use lunaris::*;

/// Live-Moon test — only runs with `--features moon-it` (set in this crate's Cargo.toml,
/// forwards through to `lunaris-storage-moon/moon-it`).
#[cfg(feature = "moon-it")]
#[tokio::test]
async fn moon_url_returns_handle_with_moon_capabilities() {
    let h = lunaris::open("moon://localhost:6380").await.expect("moon URL should parse");
    let cap = h.capabilities();
    assert!(cap.bi_temporal_native, "Moon should report bi_temporal_native=true");
    assert!(cap.graph_native);
    assert!(cap.rerank_native, "Moon should report rerank_native=true");
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768, "Moon profile pinned to 768d");
}

/// Live-Postgres test — only runs with `--features pg-it` (set in this crate's Cargo.toml,
/// forwards through to `lunaris-storage-postgres/pg-it`). The Plan-02 unconditional version
/// of this test was retired when Plan 04 wired real `sqlx` IO that requires a reachable
/// Postgres+pgvector+age+pgmq instance at the URL.
#[cfg(feature = "pg-it")]
#[tokio::test]
async fn postgres_url_returns_handle_with_postgres_capabilities() {
    let h = lunaris::open("postgres://localhost/lunaris").await.expect("postgres URL should parse");
    let cap = h.capabilities();
    assert!(!cap.bi_temporal_native, "Postgres bi-temporal is emulated");
    assert!(cap.graph_native, "AGE provides graph");
    assert!(!cap.rerank_native, "no native cross-encoder on Postgres");
    assert!(cap.queue_native, "pgmq provides queue");
    assert_eq!(cap.max_vector_dim, 1536);
}

#[tokio::test]
async fn rejects_unknown_scheme() {
    // We can't use `.expect_err()` here — `Arc<dyn StoragePort>` does not implement
    // `Debug`, so `Result::expect_err` (which formats the Ok variant on failure)
    // won't compile. Match the result by hand instead.
    match lunaris::open("rediss://foo").await {
        Ok(_) => panic!("rediss must be rejected"),
        Err(e) => {
            let s = e.to_string();
            assert!(s.contains("storage"), "error must surface as a Storage variant: {s}");
            assert!(s.contains("rediss"), "error must mention the offending scheme: {s}");
        }
    }
}

#[tokio::test]
async fn rejects_garbage_url() {
    match lunaris::open("not a url").await {
        Ok(_) => panic!("garbage must be rejected"),
        Err(e) => assert!(e.to_string().contains("storage")),
    }
}
