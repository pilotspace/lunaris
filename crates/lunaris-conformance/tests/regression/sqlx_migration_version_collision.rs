//! Plan 14-03 Task 2 — Regression test for v0.1.1 bug #1:
//! **sqlx migration version collision** between Lunaris and pgmq.
//!
//! ## Bug history
//!
//! During the EVAL-05 10K-turn HUMAN-UAT run leading up to v0.1.1 release,
//! booting `PostgresStorage` on a production-shaped Postgres (pgvector +
//! Apache AGE + pgmq) surfaced a collision between Lunaris's sqlx-managed
//! migration ordering and pgmq's own sqlx-managed schema: both tried to
//! populate `_sqlx_migrations` in the same search path, and one set
//! overwrote the other during boot. The v0.1.1 fix lives across
//! `crates/lunaris-storage-postgres/migrations/*` — pgmq is created by
//! the image's `init-extensions.sql` (before lunaris migrations run), so
//! lunaris's migration table never collides with a pgmq-owned one on
//! `pg-lunaris`. See `.planning/milestones/v0.1.1-MILESTONE-AUDIT.md`
//! (release-blocker bug list) and `.planning/ROADMAP.md` Phase 14 block.
//!
//! ## Test oracle — positive path (`pg-lunaris`)
//!
//! Invariant: `PostgresStorage::connect(&PG_URL)` succeeds AND the
//! `_sqlx_migrations` table exists with the expected row count from the
//! four lunaris migration files under
//! `crates/lunaris-storage-postgres/migrations/`.
//!
//! ## Test oracle — negative path (vanilla `postgres:16`)
//!
//! Invariant: `PostgresStorage::connect(&PG_URL)` fails at the very first
//! migration statement (`CREATE EXTENSION IF NOT EXISTS vector`) because
//! vanilla pg:16 does not ship pgvector. This is the CI-substrate-gap
//! thesis in action: CI running against vanilla pg:16 never even reaches
//! the collision path, which is why the bug slipped through in v0.1.1.
//!
//! The test captures this asymmetry via [`EXPECTED_VANILLA_ERROR`] — the
//! `integration-vanilla-pg-negative` job in `.github/workflows/integration.yml`
//! greps stderr for this substring.
//!
//! ## EXPECTED_VANILLA_ERROR — empirically locked 2026-04-23
//!
//! Booted `docker run --rm -d --name vanilla-pg16 -p 5433:5432 \
//!   -e POSTGRES_PASSWORD=lunaris -e POSTGRES_DB=lunaris -e POSTGRES_USER=lunaris postgres:16`
//! and ran `psql ... -c "CREATE EXTENSION vector;"` — observed:
//!
//! ```text
//! ERROR:  extension "vector" is not available
//! DETAIL:  Could not open extension control file ...
//! ```
//!
//! The substring `extension "vector" is not available` is therefore
//! locked. See module-level rustdoc in
//! `crates/lunaris-conformance/tests/regression.rs` for the honest
//! framing note — all 3 PG-dependent tests share this substring because
//! vanilla pg:16 fails at the first `CREATE EXTENSION` statement BEFORE
//! reaching any bug-specific setup.

#![cfg(feature = "pg-it")]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Substring asserted by the `integration-vanilla-pg-negative` CI job's
/// post-step. Must match the Rust `EXPECTED_VANILLA_ERRORS` table entry
/// in `tests/regression.rs` AND the hardcoded YAML list in
/// `.github/workflows/integration.yml`.
pub const EXPECTED_VANILLA_ERROR: &str = "extension \"vector\" is not available";

#[tokio::test]
async fn sqlx_migration_version_collision() -> anyhow::Result<()> {
    let url = match probe_backend("PG_URL", std::env::var("PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    // Positive path (pg-lunaris): connect() runs the 4-file lunaris
    // migration set against an image that has pgvector + AGE + pgmq
    // already installed via `init-extensions.sql`. pgmq-owned tables
    // (`pgmq.*`) live in a separate schema, so there is no collision.
    let storage = lunaris_storage_postgres::PostgresStorage::connect(&url).await.map_err(|e| {
        // When this fails on vanilla pg:16, the error message MUST
        // contain EXPECTED_VANILLA_ERROR for the negative-matrix
        // assertion to pass. We return the error as-is so the cargo
        // test runner prints it to stderr verbatim.
        anyhow::anyhow!(
            "PostgresStorage::connect failed: {e} — \
                 (on vanilla postgres:16 this is the expected substrate-gap \
                 signal; regression-test EXPECTED_VANILLA_ERROR = {EXPECTED_VANILLA_ERROR:?})"
        )
    })?;

    // Invariant on pg-lunaris: the lunaris sqlx migration table exists and
    // holds exactly the 4 migrations shipped under
    // `crates/lunaris-storage-postgres/migrations/`. Query the default
    // sqlx-managed `_sqlx_migrations` table; row count == migration-file
    // count proves lunaris's migrations ran in full without pgmq's own
    // sqlx state stomping on the table.
    let pool = &storage.client().pool;
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*)::bigint FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .map_err(|e| anyhow::anyhow!("query _sqlx_migrations: {e}"))?;

    // v0.1.1 ships 4 lunaris migration files; if pgmq ever stomps the
    // table, this count drops. Bound-check (>= 4) not strict-eq to
    // tolerate future additive migrations.
    assert!(
        row.0 >= 4,
        "expected >=4 lunaris migrations recorded in _sqlx_migrations, found {}",
        row.0
    );

    Ok(())
}

/// Mirror of `run_storage_postgres.rs::probe_backend` (Shared Pattern 2 —
/// duplicated per 02-04's "extract when 3rd caller asks" convention).
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
    } else {
        eprintln!("regression::sqlx_migration_version_collision: SKIP {name} (unknown URL scheme)");
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = host_port.to_socket_addrs().ok().and_then(|mut it| it.next())?;
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "regression::sqlx_migration_version_collision: SKIP {name} (TCP probe to {host_port} failed)"
            );
            None
        }
    }
}
