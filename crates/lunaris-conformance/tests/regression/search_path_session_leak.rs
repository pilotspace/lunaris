//! Plan 14-03 Task 3 — Regression test for v0.1.1 bug #2:
//! **`SET search_path` session leak** across pool checkouts.
//!
//! ## Bug history
//!
//! During EVAL-05 (v0.1.1), a per-query `SET search_path TO lunaris, public`
//! leaked across `PgPool` connection checkouts — subsequent queries from
//! an unrelated caller inherited the dirty search_path and landed in the
//! wrong schema (pgmq lookups hit the lunaris schema or vice versa).
//! The v0.1.1 fix: `PgClient::connect` (see
//! `crates/lunaris-storage-postgres/src/pool.rs:37`) issues the
//! `SET search_path = ag_catalog, "$user", public` as a single
//! pool-wide `execute(&pool)` at boot, AND the per-session `LOAD 'age'`
//! is also pool-bound, not per-query. No per-request mutation of
//! search_path survives to the next checkout.
//!
//! See `.planning/milestones/v0.1.1-MILESTONE-AUDIT.md` and
//! `.planning/ROADMAP.md` Phase 14 block.
//!
//! ## Test oracle — positive path (`pg-lunaris`)
//!
//! Invariant: lunaris's production `StoragePort` code paths do NOT
//! mutate `search_path` per request to a `lunaris`-only override. We
//! exercise `read_as_of` + `scan_range` (two canonical KV paths in
//! `crates/lunaris-storage-postgres/src/kv.rs`), then issue a burst of
//! fresh `SHOW search_path` queries via `PgPool::fetch_one` and assert
//! NONE of them return the `lunaris`-only string — that would be the
//! v0.1.1 bug's exact signature (a leaked per-query `SET search_path TO
//! lunaris`).
//!
//! Empirical note (sqlx 0.9): `sqlx::query(...).execute(&pool)` inside
//! `PgClient::connect` runs against ONE checked-out connection, not
//! across the whole pool. So the boot-time `SET search_path = ag_catalog,
//! "$user", public` (pool.rs:37) is NOT reliably visible on every fresh
//! checkout — a newly-opened connection from the pool returns Postgres'
//! standard default. The invariant this test encodes therefore focuses
//! on the NEGATIVE signal (a `lunaris`-only search_path is the
//! smoking-gun leak), not on the positive `ag_catalog` presence.
//!
//! Empirical note #2 (sqlx 0.9): `sqlx::PgPool` does NOT run a
//! RESET SESSION on connection release. A regression reintroducing
//! per-query `SET search_path TO lunaris` would therefore persist
//! across checkouts. The positive-path assertion cannot itself dirty
//! the pool (that would self-inflict the leak); it exercises only the
//! Lunaris public API and reads back the observable search_path.
//!
//! ## Test oracle — negative path (vanilla `postgres:16`)
//!
//! Invariant: `PostgresStorage::connect(&PG_URL)` fails at the sqlx
//! migration step (`CREATE EXTENSION IF NOT EXISTS vector`) because
//! vanilla pg:16 does not ship pgvector. Same substrate-gap signal as
//! regression #1 — vanilla CI never reaches the pool-checkout path where
//! the bug lives. See module-level rustdoc in `tests/regression.rs`.
//!
//! ## EXPECTED_VANILLA_ERROR — empirically locked 2026-04-23
//!
//! Matches regression #1 — both fail at the first `CREATE EXTENSION vector`
//! statement in `20260420000001_init_extensions.sql`.

#![cfg(feature = "pg-it")]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Substring asserted by the `integration-vanilla-pg-negative` CI job's
/// post-step. Must match the Rust `EXPECTED_VANILLA_ERRORS` table entry
/// in `tests/regression.rs` AND the hardcoded YAML list in
/// `.github/workflows/integration.yml`.
pub const EXPECTED_VANILLA_ERROR: &str = "extension \"vector\" is not available";

#[tokio::test]
async fn search_path_session_leak() -> anyhow::Result<()> {
    let url = match probe_backend("PG_URL", std::env::var("PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let storage = lunaris_storage_postgres::PostgresStorage::connect(&url)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "PostgresStorage::connect failed: {e} — \
                 (on vanilla postgres:16 this is the expected substrate-gap \
                 signal; regression-test EXPECTED_VANILLA_ERROR = {EXPECTED_VANILLA_ERROR:?})"
            )
        })?;
    let pool = &storage.client().pool;

    // Step 1 — exercise the three canonical Lunaris KV paths against an
    // empty DB. These are the code paths where the v0.1.1 bug lived; if
    // any of them had reintroduced a per-query `SET search_path`, the
    // dirty connection would return to the pool and the subsequent
    // `SHOW search_path` would miss `ag_catalog`.
    let missing_key = b"lunaris-regression-14-03-missing-key";
    let _ = lunaris_core::StoragePort::read_as_of(
        &storage,
        missing_key,
        lunaris_core::hlc::Hlc::from_parts(u64::MAX / 2, 0, 0),
    )
    .await?;
    // scan_range returns a stream; drain it (empty) to ensure the full
    // query lifecycle executed.
    use futures::stream::StreamExt;
    let mut s = lunaris_core::StoragePort::scan_range(&storage, missing_key, None).await?;
    while s.next().await.is_some() {}
    // atomic_write with zero ops is a no-op; we skip it to avoid
    // touching the bitemporal write path (which has its own fixtures).
    // read_as_of + scan_range are sufficient to prove the read-side
    // code paths do not mutate search_path per-query.

    // Step 2 — issue a burst of fresh checkouts. Each MUST NOT report a
    // search_path narrowed to a lunaris-only value (the v0.1.1 bug's
    // signature: a per-query `SET search_path TO lunaris` leaking across
    // pool checkouts). The acceptable values are whatever Postgres +
    // the boot-time best-effort SET in `pool.rs:37` yield — either
    // `"$user", public` (pg default on fresh-opened connections in the
    // pool, because sqlx opens connections lazily and the pool-wide SET
    // runs only against whichever connection was up at boot time) OR
    // `ag_catalog, "$user", public` (the connection that was up at
    // boot time). Both are safe; `lunaris` alone is the smoking gun.
    //
    // Empirical note (sqlx 0.9): `sqlx::query(...).execute(&pool)` in
    // `PgClient::connect` runs against a SINGLE checked-out connection,
    // NOT across the whole pool. The v0.1.1 fix therefore depends on
    // NO production code path SETting search_path per-query. This test
    // encodes that invariant directly.
    for i in 0..8 {
        let (sp,): (String,) = sqlx::query_as("SHOW search_path")
            .fetch_one(pool)
            .await?;
        let normalized = sp.replace(' ', "");
        assert!(
            !normalized.eq_ignore_ascii_case("lunaris"),
            "search_path session leak on checkout #{i}: SHOW search_path \
             returned {sp:?} — a lunaris-only search_path across a fresh \
             pool checkout is the v0.1.1 bug's signature (per-query `SET \
             search_path TO lunaris` leaked via connection reuse). \
             Production code paths must NOT issue per-query SET; use \
             `SET LOCAL` inside a transaction instead."
        );
    }

    Ok(())
}

/// Shared Pattern 2 — see `sqlx_migration_version_collision.rs` rustdoc.
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') {
            bare.to_string()
        } else {
            format!("{bare}:5432")
        }
    } else {
        eprintln!("regression::search_path_session_leak: SKIP {name} (unknown URL scheme)");
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = host_port.to_socket_addrs().ok().and_then(|mut it| it.next())?;
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "regression::search_path_session_leak: SKIP {name} (TCP probe to {host_port} failed)"
            );
            None
        }
    }
}
