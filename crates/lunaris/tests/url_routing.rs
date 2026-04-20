//! URL-scheme routing tests for `lunaris::open(url)`.
//!
//! ## Test split (after Plan 03)
//!
//! Plan 02 wired `MoonStorage::connect` to a string-only stub that always succeeded;
//! Plan 03 replaced it with a real `redis::aio::ConnectionManager::new(...)` that
//! requires a live Moon at the URL. So the tests that actually open a Moon URL now
//! depend on a running Moon — they are gated behind the `moon-it` feature.
//!
//! `PostgresStorage::connect` is still the Plan 02 stub (Plan 04 fills it), so the
//! Postgres-side capability test still runs in CI without a Postgres listener (the
//! current PostgresStorage::connect does NOT dial — it only validates the URL).
//!
//! When Plan 04 lands, `postgres_url_returns_handle_with_postgres_capabilities` will
//! also need to be gated behind a `pg-it` feature.

use lunaris::*;

/// Live-Moon test — only runs with `--features moon-it` (set in this crate's Cargo.toml,
/// forwards through to `lunaris-storage-moon/moon-it`).
#[cfg(feature = "moon-it")]
#[tokio::test]
async fn moon_url_returns_handle_with_moon_capabilities() {
    let h = lunaris::open("moon://localhost:6390").await.expect("moon URL should parse");
    let cap = h.capabilities();
    assert!(cap.bi_temporal_native, "Moon should report bi_temporal_native=true");
    assert!(cap.graph_native);
    assert!(cap.rerank_native, "Moon should report rerank_native=true");
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768, "Moon profile pinned to 768d");
}

/// Postgres connect is still a Plan-02 stub (URL-only, no dial), so this test runs
/// in CI without a Postgres listener. Plan 04 will gate this behind a `pg-it` feature
/// when the real connection lands.
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

/// Phase 1 skeleton-state proof — Postgres still returns `NotSupported` on its IO
/// methods (Plan 04 will replace these). Moved here from Plan 02's
/// `skeleton_io_methods_return_not_supported` because Plan 03 just filled in
/// MoonStorage's IO bodies — Moon no longer returns `NotSupported`.
///
/// When Plan 04 lands, this test gets removed entirely (or gated behind `pg-it` and
/// rewritten to actually exercise the real Postgres path).
#[tokio::test]
async fn postgres_skeleton_io_methods_still_return_not_supported() {
    let h = lunaris::open("postgres://localhost/lunaris").await.unwrap();
    let r = h.atomic_write(&[]).await;
    assert!(
        matches!(r, Err(StorageError::NotSupported(_))),
        "Phase 1 Postgres skeleton must return NotSupported, got {r:?}"
    );
}
