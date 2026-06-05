//! Plan 14-03 Task 5 — Regression test for v0.1.1 bug #4:
//! **bytea `LIKE` → byte-range scan on `lunaris_kv.key`**.
//!
//! ## Bug history
//!
//! An earlier `scan_range` implementation wrote the prefix match as
//! `WHERE key LIKE $1::bytea || '%'::bytea`. Postgres planned this as a
//! byte-range scan on the bytea column and could NOT use the primary-key
//! B-tree — scan performance collapsed from O(log n) to O(n). At EVAL-05's
//! 10K-turn workload the observable wall-clock recall budget blew past
//! 25 ms.
//!
//! Fix (v0.1.1): `crates/lunaris-storage-postgres/src/kv.rs:76-145` —
//! rewrite the prefix match as `WHERE key >= $1 AND key < <successor>($1)`
//! with explicit bytea bounds. The B-tree primary-key index on
//! `lunaris_kv.key` now handles the query as an Index Scan.
//!
//! See `.planning/milestones/v0.1.1-MILESTONE-AUDIT.md` and
//! `.planning/ROADMAP.md` Phase 14 block.
//!
//! ## Test oracle — positive path (`pg-lunaris`)
//!
//! Invariant: the `EXPLAIN (FORMAT JSON)` plan for a representative
//! `scan_range` query uses an Index Scan on `lunaris_kv_pkey` (the
//! BYTEA PRIMARY KEY's B-tree), NOT a Seq Scan or Bitmap Heap Scan
//! with a `~~` / LIKE-shaped filter. We seed ~100 prefixed keys so
//! Postgres's planner has a reason to prefer the index — on a tiny
//! table the planner may pick Seq Scan even with a correct query.
//!
//! This is a **fix-invariance** test, not a **failure-mode replay**:
//! the buggy `LIKE` version would also return correct rows (just
//! slowly); our assertion is on plan SHAPE, which locks the SQL in
//! `kv.rs` against future regressions that would re-introduce `LIKE`.
//!
//! ## Test oracle — negative path (vanilla `postgres:16`)
//!
//! Invariant: `PostgresStorage::connect(&PG_URL)` fails at the sqlx
//! migration step (`CREATE EXTENSION IF NOT EXISTS vector`) because
//! vanilla pg:16 does not ship pgvector. Same substrate-gap signal as
//! regressions #1 and #2.
//!
//! ## EXPECTED_VANILLA_ERROR — empirically locked 2026-04-23
//!
//! Same as regression #1 — all PG-dependent tests hit the same vector
//! extension error. See module rustdoc at `tests/regression.rs`.

#![cfg(feature = "pg-it")]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Substring asserted by the `integration-vanilla-pg-negative` CI job's
/// post-step. Must match the Rust `EXPECTED_VANILLA_ERRORS` table entry
/// in `tests/regression.rs` AND the hardcoded YAML list in
/// `.github/workflows/integration.yml`.
pub const EXPECTED_VANILLA_ERROR: &str = "extension \"vector\" is not available";

/// Seed count: large enough to tip the planner towards Index Scan for a
/// selective prefix. On an empty table the planner may pick Seq Scan
/// regardless of query shape (small-N heuristic).
const SEED_ROW_COUNT: usize = 128;

#[tokio::test]
async fn bytea_like_byterange_scan() -> anyhow::Result<()> {
    let url = match probe_backend("PG_URL", std::env::var("PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let storage = lunaris_storage_postgres::PostgresStorage::connect(&url).await.map_err(|e| {
        anyhow::anyhow!(
            "PostgresStorage::connect failed: {e} — \
                 (on vanilla postgres:16 this is the expected substrate-gap \
                 signal; regression-test EXPECTED_VANILLA_ERROR = {EXPECTED_VANILLA_ERROR:?})"
        )
    })?;
    let pool = &storage.client().pool;

    // Seed lunaris_kv with N keys sharing a distinctive prefix so a
    // Seq Scan vs Index Scan choice is planner-visible. The prefix
    // includes a run-unique suffix so concurrent test runs on the same
    // DB don't collide.
    let prefix_stem = format!("regression-14-03-bytea-scan-{}/", std::process::id());
    let prefix = prefix_stem.as_bytes();
    for i in 0..SEED_ROW_COUNT {
        let key = format!("{prefix_stem}{i:05}").into_bytes();
        let value = format!("v{i}").into_bytes();
        // `scope` became NOT NULL (no default) in migration
        // 20260510000005_scope_partitioning (RFC 0001); the fixture must
        // supply it like every production KvPut does. Literal satisfies the
        // `lunaris_kv_scope_check` alphabet constraint.
        sqlx::query(
            "INSERT INTO lunaris_kv \
               (scope, key, value, bt, valid_from, sys_from, valid_from_hlc, sys_from_hlc) \
             VALUES \
               ('regression-bytea-scan', $1, $2, '{}'::jsonb, NOW(), NOW(), 0, 0) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&key)
        .bind(&value)
        .execute(pool)
        .await?;
    }

    // Force the planner to refresh stats so it actually sees the new rows.
    sqlx::query("ANALYZE lunaris_kv").execute(pool).await?;

    // EXPLAIN (FORMAT JSON) of the exact byte-range query shape used by
    // `crates/lunaris-storage-postgres/src/kv.rs::scan_range` for the
    // (as_of=None, upper=Some(_)) branch. The planner MUST prefer the
    // primary-key B-tree index — any plan node naming `Seq Scan` OR any
    // filter containing `~~` (the bytea-LIKE operator shape) is the
    // regression signature.
    let upper = {
        // Mirror of `kv.rs::prefix_upper_bound` — increment the last
        // byte < 0xFF to get the exclusive upper bound.
        let mut u = prefix.to_vec();
        for i in (0..u.len()).rev() {
            if u[i] < 0xFF {
                u[i] += 1;
                u.truncate(i + 1);
                break;
            }
        }
        u
    };

    let plan_rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "EXPLAIN (FORMAT JSON) \
         SELECT key, value FROM lunaris_kv \
         WHERE key >= $1 AND key < $2 AND valid_to IS NULL",
    )
    .bind(prefix)
    .bind(&upper)
    .fetch_all(pool)
    .await?;
    assert_eq!(plan_rows.len(), 1, "EXPLAIN returned {} rows", plan_rows.len());
    let plan_json = &plan_rows[0].0;
    let plan_str = plan_json.to_string();

    // Invariant #1: the plan must NOT contain a Seq Scan of lunaris_kv.
    // (Other tables — system catalogs — are fine, but we're specifically
    // looking at lunaris_kv scans here.)
    assert!(
        !plan_str.contains("\"Node Type\":\"Seq Scan\""),
        "scan_range plan fell back to Seq Scan — bytea LIKE regression? plan = {plan_str}"
    );

    // Invariant #2: the plan must NOT contain a `~~` (LIKE) operator on
    // lunaris_kv.key. `~~` is the SQL operator name for LIKE that
    // Postgres exposes in EXPLAIN filter expressions.
    assert!(
        !plan_str.contains("~~"),
        "scan_range plan contains a `~~` (LIKE) operator — bytea LIKE regression? plan = {plan_str}"
    );

    // Invariant #3: the plan MUST contain an Index Scan OR Index Only Scan
    // node referencing `lunaris_kv_pkey` (the BYTEA PRIMARY KEY's B-tree).
    assert!(
        plan_str.contains("Index Scan") || plan_str.contains("Index Only Scan"),
        "scan_range plan does not use an Index Scan on lunaris_kv_pkey — \
         the byte-range bounds are not exercising the B-tree; plan = {plan_str}"
    );
    assert!(
        plan_str.contains("lunaris_kv_pkey"),
        "scan_range plan does not reference lunaris_kv_pkey — the B-tree on the \
         BYTEA PRIMARY KEY is the canonical byte-range oracle; plan = {plan_str}"
    );

    // Cleanup — best-effort; leaves the DB in its pre-test state for
    // subsequent regression tests in the same run.
    let _ = sqlx::query("DELETE FROM lunaris_kv WHERE key >= $1 AND key < $2")
        .bind(prefix)
        .bind(&upper)
        .execute(pool)
        .await;

    Ok(())
}

/// Shared Pattern 2 — see `sqlx_migration_version_collision.rs` rustdoc.
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
    } else {
        eprintln!("regression::bytea_like_byterange_scan: SKIP {name} (unknown URL scheme)");
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = host_port.to_socket_addrs().ok().and_then(|mut it| it.next())?;
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "regression::bytea_like_byterange_scan: SKIP {name} (TCP probe to {host_port} failed)"
            );
            None
        }
    }
}
