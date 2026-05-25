//! Schema bootstrap for the embedded SQLite backend.
//!
//! Idempotent `CREATE TABLE IF NOT EXISTS` DDL run once on
//! [`EmbeddedStorage::connect`](crate::EmbeddedStorage::connect). No sqlx
//! migration machinery — the embedded backend is a single, internal schema
//! version (a fresh `memory://` db every process; a file db that this code
//! owns end-to-end).
//!
//! All HLC bounds are zero-padded sortable `TEXT`; `NULL` = open-ended. The
//! `lunaris_vec` / `lunaris_graph_*` tables persist `atomic_write` ops so no
//! data is dropped before `vector_search` / `graph_traverse` are implemented.

use lunaris_core::error::StorageError;
use sqlx::SqlitePool;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS lunaris_kv (
        key        BLOB NOT NULL,
        value      BLOB NOT NULL,
        valid_from TEXT NOT NULL,
        valid_to   TEXT,
        sys_from   TEXT NOT NULL,
        sys_to     TEXT,
        bt         TEXT NOT NULL,
        PRIMARY KEY (key, sys_from)
    )",
    "CREATE INDEX IF NOT EXISTS lunaris_kv_key_live ON lunaris_kv (key) WHERE sys_to IS NULL",
    "CREATE TABLE IF NOT EXISTS lunaris_vec (
        idx        TEXT NOT NULL,
        id         BLOB NOT NULL,
        embedding  TEXT NOT NULL,
        metadata   TEXT NOT NULL,
        sys_from   TEXT NOT NULL,
        sys_to     TEXT,
        PRIMARY KEY (idx, id, sys_from)
    )",
    "CREATE TABLE IF NOT EXISTS lunaris_graph_node (
        graph      TEXT NOT NULL,
        id         BLOB NOT NULL,
        label      TEXT NOT NULL,
        props      TEXT NOT NULL,
        sys_from   TEXT NOT NULL,
        sys_to     TEXT,
        PRIMARY KEY (graph, id, sys_from)
    )",
    "CREATE TABLE IF NOT EXISTS lunaris_graph_edge (
        graph      TEXT NOT NULL,
        src        BLOB NOT NULL,
        dst        BLOB NOT NULL,
        rel        TEXT NOT NULL,
        props      TEXT NOT NULL,
        sys_from   TEXT NOT NULL,
        sys_to     TEXT,
        PRIMARY KEY (graph, src, dst, rel, sys_from)
    )",
    // HOOK-05: idempotency sidecar table.
    // `lsn` stored as two INTEGER columns for exact round-trip (no FromStr needed).
    // `INSERT OR IGNORE` means duplicate dedupe keys are silently no-ops (T-24-03-06 race-safe).
    "CREATE TABLE IF NOT EXISTS lunaris_dedupe (
        scope      TEXT    NOT NULL,
        dedupe_key TEXT    NOT NULL,
        lsn_wall   INTEGER NOT NULL,
        lsn_ctr    INTEGER NOT NULL,
        created_at TEXT    NOT NULL,
        PRIMARY KEY (scope, dedupe_key)
    )",
];

/// Create every table/index if absent.
pub(crate) async fn init(pool: &SqlitePool) -> Result<(), StorageError> {
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(pool)
        .await
        .map_err(|e| StorageError::Backend(format!("embedded(sqlite): {e}")))?;
    for stmt in DDL {
        sqlx::query(*stmt)
            .execute(pool)
            .await
            .map_err(|e| StorageError::Backend(format!("embedded(sqlite) schema init: {e}")))?;
    }
    Ok(())
}
