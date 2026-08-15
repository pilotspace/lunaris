//! URL-scheme routing tests for `lunaris::open(url)`.
//!
//! ## Test split (after Plan 04)
//!
//! Plan 02 wired both `MoonStorage::connect` and `PostgresStorage::connect` to
//! URL-only stubs that always succeeded; Plans 03 / 04 replaced them with real
//! IO. The test that actually opens a Moon URL therefore depends on a running
//! backend at that URL — it is gated behind the `moon-it` feature. Its
//! Postgres sibling was deleted in 0.7.0 with that backend.
//!
//! Tests for invalid URL schemes (rediss / garbage) still run in CI without any backend,
//! because the URL allowlist rejects them BEFORE any backend constructor dials out.
//! The RETIRED schemes (`postgres://`, `sqlite://`, `memory://`) are covered by
//! `lunaris::open`'s own unit tests, which assert the migration prose.

// `lunaris::*` is only needed when the moon-it gate is on; without it the
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
    assert!(
        !cap.bi_temporal_native,
        "Moon HGET does not support KV AS_OF; graph/FT temporal surfaces are separate"
    );
    assert!(cap.graph_native);
    assert!(cap.rerank_native, "Moon should report rerank_native=true");
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768, "Moon profile pinned to 768d");
}

// `postgres_url_returns_handle_with_postgres_capabilities` (gated on `pg-it`)
// was deleted in 0.7.0 with `lunaris-storage-postgres`. `postgres://` now
// routes to the retired-scheme error — see `lunaris::open`'s unit tests.

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
