//! Wave 1.C — concurrent-handles regression test.
//!
//! Two complementary tests:
//!
//! 1. `wal_pragma_is_applied` (structural RED→GREEN):
//!    Queries `PRAGMA journal_mode` immediately after `connect()` and asserts
//!    the value is `"wal"`.  Before the fix it returns `"delete"` — that is the
//!    reliable RED signal.  After the fix it returns `"wal"` — GREEN.
//!
//! 2. `concurrent_writers_no_busy_under_wal` (behavioural GREEN):
//!    Opens two `EmbeddedStorage` handles on the SAME sqlite file from two
//!    tokio tasks, writes 100 episodes each concurrently, asserts:
//!      (a) zero `SQLITE_BUSY` / backend errors
//!      (b) all 200 episodes visible to a third reader
//!
//! ## Why WAL matters
//!
//! Without `journal_mode=WAL` + `busy_timeout=5000` two concurrent writers on
//! the same file race over the exclusive write-lock.  The default DELETE journal
//! has `busy_timeout=0`, so the second writer gets `SQLITE_BUSY` immediately if
//! it arrives while the first transaction is open.  WAL lets the coordinator
//! queue the second writer for up to 5 s — all 200 writes succeed cleanly.
//!
//! ## Key design notes
//!
//! - Tasks use distinct scope-label prefixes (`writer-a` vs `writer-b`) so keys
//!   from both tasks never collide, even if both `HlcClock` instances
//!   (node_id=0) tick at the same wall_ms.  Colliding keys would fire the PK
//!   constraint rather than SQLITE_BUSY, masking the real bug.
//! - The row count uses a direct pool query rather than `scan_range` to keep
//!   the test focused on the storage invariant.

#![allow(clippy::unwrap_used)]

use lunaris_core::keyspace::episode_key;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort as _;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_embedded::EmbeddedStorage;
use tempfile::TempDir;
use ulid::Ulid;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Write `n` episodes sequentially under a label-scoped set of unique keys.
async fn write_n_episodes(
    storage: EmbeddedStorage,
    label: &str,
    n: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let scope = Scope::new(label).expect("label is a valid scope");
    for _ in 0..n {
        let id = Ulid::new();
        let key = episode_key(&scope, id);
        storage
            .atomic_write(
                &scope,
                &[WriteOp::KvPut { key, value: b"episode-payload".to_vec() }],
            )
            .await
            .map_err(|e| format!("atomic_write failed (SQLITE_BUSY?): {e}"))?;
    }
    Ok(())
}

// ── Test 1: structural — RED before fix, GREEN after ─────────────────────────

/// Verify that every new file-backed connection has `journal_mode = wal`.
///
/// This is the canonical RED→GREEN pivot for Wave 1.C:
///   - **Before** the fix: `PRAGMA journal_mode` returns `"delete"` → assertion
///     panics with `"expected journal_mode=wal, got delete"`.
///   - **After** the fix: returns `"wal"` → test passes.
///
/// `memory://` is excluded here because sqlite always reports `"memory"` for
/// in-memory databases regardless of the requested journal mode; that behaviour
/// is verified by `memory_mode_connect_succeeds` below.
#[tokio::test]
async fn wal_pragma_is_applied() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("pragma_check.db");
    let url = format!("sqlite://{}", db_path.display());

    let storage = EmbeddedStorage::connect(&url).await.expect("connect");

    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(storage.pool())
        .await
        .expect("PRAGMA journal_mode");

    assert_eq!(mode, "wal", "expected journal_mode=wal, got {mode}");

    let timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(storage.pool())
        .await
        .expect("PRAGMA busy_timeout");

    assert_eq!(timeout, 5000, "expected busy_timeout=5000 ms, got {timeout}");
}

/// Verify that `memory://` connects cleanly (WAL PRAGMA is a no-op on
/// in-memory databases — sqlite reports `"memory"` as the effective mode,
/// not an error).
#[tokio::test]
async fn memory_mode_connect_succeeds() {
    let storage = EmbeddedStorage::connect("memory://").await.expect("connect memory://");

    // `PRAGMA journal_mode` on an in-memory db always returns "memory"
    // regardless of the requested mode. Verify it doesn't error.
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(storage.pool())
        .await
        .expect("PRAGMA journal_mode on memory://");

    // The effective mode may be "memory" or "wal" depending on sqlite build;
    // what matters is that connect() did not return an error.
    assert!(
        !mode.is_empty(),
        "PRAGMA journal_mode must return a non-empty string for memory://"
    );
}

// ── Test 2: behavioural — two handles, 200 writes, zero BUSY ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_no_busy_under_wal() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let url = format!("sqlite://{}", db_path.display());

    // Two independent handles — like two Claude Code editor processes pointing
    // at the same ~/.lunaris/<scope>.db file.
    let h1 = EmbeddedStorage::connect(&url).await.expect("connect h1");
    let h2 = EmbeddedStorage::connect(&url).await.expect("connect h2");

    let task_a = tokio::spawn(write_n_episodes(h1, "writer-a", 100));
    let task_b = tokio::spawn(write_n_episodes(h2, "writer-b", 100));

    let (res_a, res_b) = tokio::join!(task_a, task_b);
    res_a.expect("task_a did not panic").expect("task_a: no SQLITE_BUSY");
    res_b.expect("task_b did not panic").expect("task_b: no SQLITE_BUSY");

    // Third independent reader — proves WAL makes writes visible cross-handle.
    let h3 = EmbeddedStorage::connect(&url).await.expect("connect h3");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lunaris_kv WHERE sys_to IS NULL")
            .fetch_one(h3.pool())
            .await
            .expect("count query");

    assert_eq!(count, 200, "expected 200 live episodes (100 from each writer); got {count}");
}
