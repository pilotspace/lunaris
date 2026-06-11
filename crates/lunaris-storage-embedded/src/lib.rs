//! `EmbeddedStorage` — a zero-dependency [`StoragePort`] backed by SQLite.
//!
//! Two URL forms route here from `lunaris::open` / `Lunaris::open`:
//!
//! - `memory://` — an in-process, ephemeral database (one pinned connection to
//!   `sqlite::memory:`). The "just trying it" backend: no Docker, no Postgres,
//!   no Moon.
//! - `sqlite:///abs/path` / `sqlite://rel/path` / `sqlite:path` — file-backed
//!   and durable; the file is created if missing.
//!
//! ## What's implemented (onboarding-overhaul phase 1, skeleton)
//!
//! - bi-temporal MVCC KV: [`StoragePort::atomic_write`] /
//!   [`StoragePort::read_as_of`] / [`StoragePort::scan_range`]. The
//!   atomic-write-once invariant holds — exactly one SQLite transaction per
//!   `atomic_write`, all-or-nothing.
//! - `atomic_write` also *persists* `VectorUpsert` / `GraphNode` / `GraphEdge`
//!   ops into side tables so no data is dropped, but `vector_search` /
//!   `graph_traverse` still return [`StorageError::NotSupported`] until the
//!   brute-force cosine / adjacency-walk impls land.
//! - queue (`publish` / `subscribe` / `queue_depth`) and [`KeywordPort`] return
//!   `NotSupported` for now.
//!
//! ## Bi-temporal encoding
//!
//! HLC stamps are stored as zero-padded, lexically-sortable `TEXT`
//! (`{wall_ms:020}.{counter:010}.{node_id:05}`); an open-ended bound is `NULL`.
//! The visibility predicate mirrors the Postgres backend:
//!
//! ```sql
//!   valid_from <= ?as_of AND (valid_to IS NULL OR ?as_of < valid_to)
//!   AND sys_from <= ?as_of AND (sys_to IS NULL OR ?as_of < sys_to)
//! ```
//!
//! A `None` `as_of` ("live") collapses to `valid_to IS NULL AND sys_to IS NULL`.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::{Hlc, HlcClock};
use lunaris_core::keyspace::parse_scope_from_key;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::capabilities::StorageCapabilities;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row as LRow, ScopePage, VectorHit, WriteOp,
};
use sqlx::Row as _;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

mod schema;
mod vector;

/// SQLite-backed embedded storage handle. Cheap to clone (`Arc`-shared pool).
#[derive(Clone)]
pub struct EmbeddedStorage {
    pool: SqlitePool,
    clock: Arc<HlcClock>,
}

impl std::fmt::Debug for EmbeddedStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedStorage").finish_non_exhaustive()
    }
}

impl EmbeddedStorage {
    /// Open the embedded backend for `url` (`memory://` or `sqlite:…`),
    /// creating the schema if needed.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let (opts, pin_single) = parse_url(url)?;
        let mut pool_opts = SqlitePoolOptions::new();
        if pin_single {
            // `sqlite::memory:` gives each connection its own database — pin the
            // pool to one connection so every query sees the same data.
            pool_opts = pool_opts.max_connections(1);
        }
        let pool = pool_opts.connect_with(opts).await.map_err(backend)?;
        schema::init(&pool).await?;
        Ok(Self { pool, clock: HlcClock::new(0) })
    }

    /// Borrow the underlying pool (used by tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

fn backend(e: sqlx::Error) -> StorageError {
    StorageError::Backend(format!("embedded(sqlite): {e}"))
}

/// Apply the WAL PRAGMA bundle to `SqliteConnectOptions`.
///
/// Every file-backed connection must have:
///
/// - `journal_mode=WAL` — multi-reader / single-writer with checkpoints;
///   enables concurrent `EmbeddedStorage` handles on the same
///   `~/.lunaris/<scope>.db` file (e.g. two Claude Code editor windows)
///   without SQLITE_BUSY.
/// - `busy_timeout=5000` — 5 s patience instead of the default 0 ms
///   immediate-SQLITE_BUSY return.
/// - `synchronous=NORMAL` — WAL-recommended setting; FULL is correct only
///   for the rollback-journal (DELETE) mode.
///
/// For `memory://` (in-memory databases) sqlite ignores `journal_mode=WAL`
/// and returns `"memory"` as the effective mode — no error is raised and the
/// other PRAGMAs apply normally.  We apply the same options unconditionally
/// rather than branching; the no-op behaviour on in-memory DBs is documented
/// by sqlite and intentional.
fn wal_pragmas(opts: SqliteConnectOptions) -> SqliteConnectOptions {
    use std::time::Duration;
    opts.journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal)
}

/// Parse a Lunaris embedded-backend URL into `(SqliteConnectOptions, pin_single)`.
fn parse_url(url: &str) -> Result<(SqliteConnectOptions, bool), StorageError> {
    if url == "memory://" || url.starts_with("memory://") {
        // `?cache=shared` is unnecessary because we pin a single connection.
        // WAL is a no-op on in-memory DBs (sqlite reports "memory" as the
        // effective journal mode) but applying wal_pragmas unconditionally
        // keeps the code path identical and avoids a silent divergence.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").map_err(backend)?;
        return Ok((wal_pragmas(opts), true));
    }
    if let Some(rest) = url.strip_prefix("sqlite:") {
        // Accept `sqlite:///abs`, `sqlite://rel`, and bare `sqlite:rel`.
        let path = rest.trim_start_matches("//");
        if path.is_empty() || path == "/" {
            return Err(StorageError::UnsupportedScheme(format!(
                "embedded: empty sqlite path in {url:?} (use `memory://` or `sqlite:///path`)"
            )));
        }
        let opts = SqliteConnectOptions::new().filename(path).create_if_missing(true);
        return Ok((wal_pragmas(opts), false));
    }
    Err(StorageError::UnsupportedScheme(format!(
        "embedded: {url:?} is not a `memory://` or `sqlite:` URL"
    )))
}

// ---- HLC <-> sortable TEXT ------------------------------------------------

/// Encode an HLC as a zero-padded, lexically-sortable string.
fn hlc_key(h: Hlc) -> String {
    format!("{:020}.{:010}.{:05}", h.wall_ms, h.counter, h.node_id)
}

fn bt_open(h: Hlc) -> BiTemporal {
    BiTemporal { valid: (h, None), sys: (h, None) }
}

// ---- atomic_write helpers -------------------------------------------------

async fn close_open_kv(
    tx: &mut sqlx::SqliteConnection,
    key: &[u8],
    at: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE lunaris_kv SET sys_to = ?1, valid_to = ?1 WHERE key = ?2 AND sys_to IS NULL",
    )
    .bind(at)
    .bind(key)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    Ok(())
}

#[async_trait]
impl StoragePort for EmbeddedStorage {
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        // One HLC tick → one commit point → one transaction. All ops share the
        // commit stamp. Scope is carried inside the KV keys (the
        // `lunaris:{scope}:…` keyspace), mirroring the Postgres `lunaris_kv`
        // which also has no separate scope column.
        let h = self.clock.tick();
        let hk = hlc_key(h);
        let mut tx = self.pool.begin().await.map_err(backend)?;
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    close_open_kv(&mut tx, key, &hk).await?;
                    let bt = serde_json::to_string(&bt_open(h))?;
                    sqlx::query(
                        "INSERT OR REPLACE INTO lunaris_kv \
                         (key, value, valid_from, valid_to, sys_from, sys_to, bt) \
                         VALUES (?1, ?2, ?3, NULL, ?3, NULL, ?4)",
                    )
                    .bind(key)
                    .bind(value)
                    .bind(&hk)
                    .bind(&bt)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                }
                WriteOp::KvDelete { key } => {
                    // Tombstone-by-absence: close the open version, insert nothing.
                    close_open_kv(&mut tx, key, &hk).await?;
                }
                WriteOp::VectorUpsert { index, id, embedding, metadata } => {
                    // RFC 0001 Wave A.1 — scope-prefix the stored idx so
                    // `vector_search` can isolate rows at the SQL boundary
                    // (mirrors Moon's `ft_index_name(scope, kind)` convention).
                    let scoped_idx = vector::scoped_idx(scope, index);
                    sqlx::query(
                        "UPDATE lunaris_vec SET sys_to = ?1 \
                         WHERE idx = ?2 AND id = ?3 AND sys_to IS NULL",
                    )
                    .bind(&hk)
                    .bind(&scoped_idx)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                    sqlx::query(
                        "INSERT OR REPLACE INTO lunaris_vec \
                         (idx, id, embedding, metadata, sys_from, sys_to) \
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                    )
                    .bind(&scoped_idx)
                    .bind(id)
                    .bind(serde_json::to_string(embedding)?)
                    .bind(serde_json::to_string(metadata)?)
                    .bind(&hk)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                }
                WriteOp::GraphNode { graph, id, label, props } => {
                    sqlx::query(
                        "UPDATE lunaris_graph_node SET sys_to = ?1 \
                         WHERE graph = ?2 AND id = ?3 AND sys_to IS NULL",
                    )
                    .bind(&hk)
                    .bind(graph)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                    sqlx::query(
                        "INSERT OR REPLACE INTO lunaris_graph_node \
                         (graph, id, label, props, sys_from, sys_to) \
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                    )
                    .bind(graph)
                    .bind(id)
                    .bind(label)
                    .bind(serde_json::to_string(props)?)
                    .bind(&hk)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                }
                WriteOp::GraphEdge { graph, src, dst, rel, props } => {
                    sqlx::query(
                        "UPDATE lunaris_graph_edge SET sys_to = ?1 \
                         WHERE graph = ?2 AND src = ?3 AND dst = ?4 AND rel = ?5 AND sys_to IS NULL",
                    )
                    .bind(&hk)
                    .bind(graph)
                    .bind(src)
                    .bind(dst)
                    .bind(rel)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                    sqlx::query(
                        "INSERT OR REPLACE INTO lunaris_graph_edge \
                         (graph, src, dst, rel, props, sys_from, sys_to) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                    )
                    .bind(graph)
                    .bind(src)
                    .bind(dst)
                    .bind(rel)
                    .bind(serde_json::to_string(props)?)
                    .bind(&hk)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                }
                // `WriteOp` is `#[non_exhaustive]`: a variant we don't know how
                // to persist must abort the whole batch, never silently drop.
                _ => {
                    return Err(StorageError::NotSupported(
                        "embedded backend: unrecognised WriteOp variant",
                    ));
                }
            }
        }
        tx.commit().await.map_err(backend)?;
        Ok(Lsn::from_hlc(h))
    }

    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        scope: &Scope,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        vector::vector_search(&self.pool, scope, index, query, k, filter, as_of, rerank).await
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported(
            "embedded backend: graph_traverse not yet implemented (adjacency walk pending)",
        ))
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        let lower = prefix.to_vec();
        let upper = prefix_upper_bound(prefix);
        let rows = match (as_of, upper) {
            (Some(t), Some(u)) => {
                let ts = hlc_key(t);
                sqlx::query(
                    "SELECT key, value FROM lunaris_kv \
                     WHERE key >= ?1 AND key < ?2 \
                       AND valid_from <= ?3 AND (valid_to IS NULL OR ?3 < valid_to) \
                       AND sys_from   <= ?3 AND (sys_to   IS NULL OR ?3 < sys_to) \
                     ORDER BY key, valid_from DESC",
                )
                .bind(&lower)
                .bind(&u)
                .bind(&ts)
                .fetch_all(&self.pool)
                .await
            }
            (Some(t), None) => {
                let ts = hlc_key(t);
                sqlx::query(
                    "SELECT key, value FROM lunaris_kv \
                     WHERE key >= ?1 \
                       AND valid_from <= ?2 AND (valid_to IS NULL OR ?2 < valid_to) \
                       AND sys_from   <= ?2 AND (sys_to   IS NULL OR ?2 < sys_to) \
                     ORDER BY key, valid_from DESC",
                )
                .bind(&lower)
                .bind(&ts)
                .fetch_all(&self.pool)
                .await
            }
            (None, Some(u)) => {
                sqlx::query(
                    "SELECT key, value FROM lunaris_kv \
                     WHERE key >= ?1 AND key < ?2 AND valid_to IS NULL AND sys_to IS NULL \
                     ORDER BY key",
                )
                .bind(&lower)
                .bind(&u)
                .fetch_all(&self.pool)
                .await
            }
            (None, None) => {
                sqlx::query(
                    "SELECT key, value FROM lunaris_kv \
                     WHERE key >= ?1 AND valid_to IS NULL AND sys_to IS NULL \
                     ORDER BY key",
                )
                .bind(&lower)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(backend)?;

        // De-dup to the latest visible version per key (the DESC ordering puts
        // it first), then materialise — v0 buffers like the Postgres backend.
        let mut out: Vec<Result<(Bytes, Bytes), StorageError>> = Vec::with_capacity(rows.len());
        let mut last_key: Option<Vec<u8>> = None;
        for r in rows {
            let k: Vec<u8> = r.try_get("key").map_err(backend)?;
            if last_key.as_deref() == Some(k.as_slice()) {
                continue;
            }
            let v: Vec<u8> = r.try_get("value").map_err(backend)?;
            last_key = Some(k.clone());
            out.push(Ok((Bytes::from(k), Bytes::from(v))));
        }
        Ok(Box::pin(stream::iter(out)))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<LRow<Bytes>>, StorageError> {
        let ts = hlc_key(as_of);
        let row_opt = sqlx::query(
            "SELECT key, value, bt FROM lunaris_kv \
             WHERE key = ?1 \
               AND valid_from <= ?2 AND (valid_to IS NULL OR ?2 < valid_to) \
               AND sys_from   <= ?2 AND (sys_to   IS NULL OR ?2 < sys_to) \
             ORDER BY valid_from DESC LIMIT 1",
        )
        .bind(key)
        .bind(&ts)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;

        Ok(row_opt.map(|r| {
            let k: Vec<u8> = r.try_get("key").unwrap_or_default();
            let v: Vec<u8> = r.try_get("value").unwrap_or_default();
            let bt_str: String = r.try_get("bt").unwrap_or_default();
            let bt: BiTemporal =
                serde_json::from_str(&bt_str).unwrap_or_else(|_| bt_open(Hlc::ZERO));
            LRow { key: k, value: Bytes::from(v), bt }
        }))
    }

    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported(
            "embedded backend: publish not yet implemented (in-table SKIP-LOCKED queue pending)",
        ))
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported(
            "embedded backend: subscribe not yet implemented (in-table SKIP-LOCKED queue pending)",
        ))
    }

    async fn queue_depth(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("embedded backend: queue_depth not yet implemented"))
    }

    /// Embedded backend `list_scopes`: scans the `lunaris_kv` table for live
    /// rows whose key matches the `lunaris:{scope}:…` convention, parses out
    /// the scope segment via [`parse_scope_from_key`], dedupes, sorts
    /// ascending, and pages.
    ///
    /// ## Cursor format
    /// The cursor is the last scope string emitted in the previous page,
    /// stored verbatim. It is opaque to the caller (Q-U1 lock); callers
    /// MUST pass it back unchanged. Resume position: the smallest scope
    /// strictly greater than the cursor.
    ///
    /// ## Failure modes
    /// - SQL failure → `StorageError::Backend`
    /// - Malformed key in storage → silently skipped (a key that fails
    ///   `parse_scope_from_key` cannot have been written via the Lunaris
    ///   `atomic_write` path, but external KV puts may write anything;
    ///   silently skipping is more forgiving than failing the page).
    /// - `limit == 0` → returns an empty page with no cursor (no work,
    ///   no contract violation; matches Postgres/Moon `LIMIT 0` semantics).
    async fn list_scopes(
        &self,
        prefix: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ScopePage, StorageError> {
        if limit == 0 {
            return Ok(ScopePage::default());
        }
        // `lunaris_kv` rows track open versions via `sys_to IS NULL`. We only
        // want currently-live keys — deleted scopes (tombstoned-by-absence
        // via `close_open_kv`) MUST not appear in `list_scopes`.
        let rows = sqlx::query("SELECT DISTINCT key FROM lunaris_kv WHERE sys_to IS NULL")
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;

        let mut scopes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for r in rows {
            let k: Vec<u8> = r.try_get("key").map_err(backend)?;
            let Some(scope) = parse_scope_from_key(&k) else {
                continue; // non-Lunaris key; skip.
            };
            scopes.insert(scope.as_str().to_string());
        }

        // Apply prefix filter (case-sensitive, byte-prefix).
        let mut filtered: Vec<String> =
            scopes.into_iter().filter(|s| prefix.is_none_or(|p| s.starts_with(p))).collect();

        // Apply cursor: skip everything <= cursor (resume strictly greater).
        if let Some(after) = cursor {
            match filtered.iter().position(|s| s.as_str() > after) {
                Some(idx) => {
                    filtered.drain(..idx);
                }
                None => {
                    // Cursor points at or past the largest scope — nothing left.
                    filtered.clear();
                }
            }
        }

        // Page.
        let (page, next_cursor) = if filtered.len() > limit {
            let page: Vec<_> = filtered.drain(..limit).collect();
            let last = page.last().cloned();
            (page, last)
        } else {
            (filtered, None)
        };

        // Convert validated strings back into Scope (round-trips through the
        // regex; cannot fail because we got them from parse_scope_from_key
        // which itself called Scope::new — defensive expect anyway).
        let scopes: Vec<Scope> = page
            .into_iter()
            .map(|s| {
                Scope::new(&s).expect("parse_scope_from_key produced a valid scope by construction")
            })
            .collect();

        Ok(ScopePage { scopes, next_cursor })
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true, // first-class MVCC columns, same as the emulated Postgres path
            graph_native: false,      // graph_traverse pending
            rerank_native: false,
            queue_native: false, // queue pending
            max_vector_dim: 4096,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
        }
    }

    // ── HOOK-05: idempotency trait overrides ─────────────────────────────────

    /// SQLite-backed dedupe lookup by (scope, dedupe_key) composite PK.
    ///
    /// Returns `Ok(Some(lsn))` if the key was previously committed, `Ok(None)` otherwise.
    /// The `lunaris_dedupe` table uses two INTEGER columns (`lsn_wall`, `lsn_ctr`) to
    /// avoid a `FromStr` round-trip through the `{wall_ms}:{counter}` display format.
    async fn lookup_by_dedupe_key(
        &self,
        scope: &Scope,
        dedupe_key: &str,
    ) -> Result<Option<Lsn>, StorageError> {
        let row = sqlx::query(
            "SELECT lsn_wall, lsn_ctr FROM lunaris_dedupe \
             WHERE scope = ? AND dedupe_key = ?",
        )
        .bind(scope.as_str())
        .bind(dedupe_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;

        match row {
            None => Ok(None),
            Some(r) => {
                let wall_ms: i64 = r.try_get("lsn_wall").map_err(backend)?;
                let counter: i64 = r.try_get("lsn_ctr").map_err(backend)?;
                Ok(Some(Lsn { wall_ms: wall_ms as u64, counter: counter as u32 }))
            }
        }
    }

    /// SQLite-backed dedupe INSERT OR IGNORE after a successful `atomic_write`.
    ///
    /// `INSERT OR IGNORE` means that if the key already exists (e.g. two concurrent
    /// hook processes), the second insert is silently dropped — safe because the
    /// composite PK (scope, dedupe_key) is unique.
    async fn insert_dedupe_key(
        &self,
        scope: &Scope,
        dedupe_key: &str,
        lsn: Lsn,
    ) -> Result<(), StorageError> {
        let created_at = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT OR IGNORE INTO lunaris_dedupe \
             (scope, dedupe_key, lsn_wall, lsn_ctr, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(scope.as_str())
        .bind(dedupe_key)
        .bind(lsn.wall_ms as i64)
        .bind(lsn.counter as i64)
        .bind(created_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

#[async_trait]
impl KeywordPort for EmbeddedStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Err(StorageError::NotSupported(
            "embedded backend: keyword_search not yet implemented (FTS5 BM25 pending)",
        ))
    }
}

/// Exclusive upper bound for a byte-wise prefix scan — `None` when `prefix` is
/// empty or all-`0xFF` (scan is open-ended above). Mirrors the Postgres backend.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 0xFF {
            out[i] += 1;
            out.truncate(i + 1);
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn mem() -> EmbeddedStorage {
        EmbeddedStorage::connect("memory://").await.expect("open memory backend")
    }

    fn put(key: &[u8], val: &[u8]) -> WriteOp {
        WriteOp::KvPut { key: key.to_vec(), value: val.to_vec() }
    }

    #[tokio::test]
    async fn rejects_unknown_scheme() {
        assert!(matches!(
            EmbeddedStorage::connect("postgres://x/y").await,
            Err(StorageError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            EmbeddedStorage::connect("sqlite:").await,
            Err(StorageError::UnsupportedScheme(_))
        ));
    }

    #[tokio::test]
    async fn kv_roundtrip_and_lsn_monotonic() {
        let s = mem().await;
        let l1 = s.atomic_write(&Scope::dev(), &[put(b"k", b"v1")]).await.expect("write 1");
        let l2 = s.atomic_write(&Scope::dev(), &[put(b"k", b"v2")]).await.expect("write 2");
        assert!(l2 > l1, "Lsn must be monotonic: {l2:?} !> {l1:?}");

        let now = s.clock.tick();
        let row = s.read_as_of(&Scope::dev(), b"k", now).await.expect("read").expect("present");
        assert_eq!(&row.value[..], b"v2");
    }

    #[tokio::test]
    async fn bitemporal_snapshot_sees_old_version() {
        let s = mem().await;
        let l1 = s.atomic_write(&Scope::dev(), &[put(b"k", b"v1")]).await.expect("w1");
        let at_v1 = Hlc { wall_ms: l1.wall_ms, counter: l1.counter, node_id: 0 };
        s.atomic_write(&Scope::dev(), &[put(b"k", b"v2")]).await.expect("w2");

        let old =
            s.read_as_of(&Scope::dev(), b"k", at_v1).await.expect("read@v1").expect("present");
        assert_eq!(&old.value[..], b"v1", "snapshot at v1's stamp must see v1");

        let live = s
            .read_as_of(&Scope::dev(), b"k", s.clock.tick())
            .await
            .expect("read live")
            .expect("present");
        assert_eq!(&live.value[..], b"v2");
    }

    #[tokio::test]
    async fn delete_tombstones_but_history_remains() {
        let s = mem().await;
        let l1 = s.atomic_write(&Scope::dev(), &[put(b"k", b"v1")]).await.expect("w1");
        let at_v1 = Hlc { wall_ms: l1.wall_ms, counter: l1.counter, node_id: 0 };
        s.atomic_write(&Scope::dev(), &[WriteOp::KvDelete { key: b"k".to_vec() }])
            .await
            .expect("del");

        assert!(
            s.read_as_of(&Scope::dev(), b"k", s.clock.tick()).await.expect("read live").is_none(),
            "deleted key must be absent live"
        );
        assert!(
            s.read_as_of(&Scope::dev(), b"k", at_v1).await.expect("read@v1").is_some(),
            "history before the delete must remain readable"
        );
    }

    #[tokio::test]
    async fn scan_range_returns_latest_per_key_under_prefix() {
        let s = mem().await;
        s.atomic_write(
            &Scope::dev(),
            &[put(b"lunaris:_dev:episode:a", b"A1"), put(b"lunaris:_dev:episode:b", b"B1")],
        )
        .await
        .expect("seed");
        s.atomic_write(&Scope::dev(), &[put(b"lunaris:_dev:episode:a", b"A2")])
            .await
            .expect("update a");
        s.atomic_write(&Scope::dev(), &[put(b"other:key", b"X")]).await.expect("noise");

        let mut stream =
            s.scan_range(&Scope::dev(), b"lunaris:_dev:episode:", None).await.expect("scan");
        let mut got = Vec::new();
        while let Some(item) = stream.next().await {
            let (k, v) = item.expect("row");
            got.push((
                String::from_utf8_lossy(&k).into_owned(),
                String::from_utf8_lossy(&v).into_owned(),
            ));
        }
        assert_eq!(
            got,
            vec![
                ("lunaris:_dev:episode:a".to_string(), "A2".to_string()),
                ("lunaris:_dev:episode:b".to_string(), "B1".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn vector_upsert_is_persisted_and_searchable() {
        let s = mem().await;
        s.atomic_write(
            &Scope::dev(),
            &[WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: b"id1".to_vec(),
                embedding: vec![0.1, 0.2, 0.3],
                metadata: serde_json::json!({"src": "t"}),
            }],
        )
        .await
        .expect("vector upsert persists");

        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM lunaris_vec")
            .fetch_one(s.pool())
            .await
            .expect("count");
        assert_eq!(n, 1);

        // Wave A.1 T2: vector_search is now implemented — brute-force cosine.
        // The upserted embedding [0.1, 0.2, 0.3] with identical probe gives
        // cosine = 1.0 (exact match).
        let hits = s
            .vector_search(&Scope::dev(), "chunks", &[0.1, 0.2, 0.3], 1, None, None, false)
            .await
            .expect("vector_search must succeed");
        assert_eq!(hits.len(), 1, "must find the upserted vector");
        assert!((hits[0].score - 1.0).abs() < 1e-5, "exact-match cosine must be ~1.0");
    }

    #[tokio::test]
    async fn capabilities_are_the_embedded_profile() {
        let c = mem().await.capabilities();
        assert!(c.bi_temporal_native);
        assert!(!c.graph_native);
        assert!(!c.queue_native);
        assert!(!c.native_rrf);
    }

    // ----------------------------------------------------------------------
    // list_scopes (Q-U1 lock: opaque cursor; Q-U2 lock: lazy SCAN-parse)
    // ----------------------------------------------------------------------

    /// Seed three distinct scopes via the canonical key format. Each scope
    /// writes one episode key so `list_scopes` has something to dedupe.
    async fn seed_three_scopes(s: &EmbeddedStorage) {
        use lunaris_core::keyspace::episode_key;
        use ulid::Ulid;
        let id = Ulid::new();
        for name in ["alpha", "beta.team", "gamma_42"] {
            let scope = Scope::new(name).expect("valid scope");
            let key = episode_key(&scope, id);
            s.atomic_write(&scope, &[WriteOp::KvPut { key, value: b"x".to_vec() }])
                .await
                .expect("seed");
        }
    }

    #[tokio::test]
    async fn list_scopes_returns_all_scopes_sorted_when_unpaged() {
        let s = mem().await;
        seed_three_scopes(&s).await;
        let page = s.list_scopes(None, 100, None).await.expect("list");
        let names: Vec<&str> = page.scopes.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta.team", "gamma_42"]);
        assert!(page.next_cursor.is_none(), "single page must not return a cursor");
    }

    #[tokio::test]
    async fn list_scopes_dedupes_when_one_scope_has_many_keys() {
        // Write two keys under the same scope. `list_scopes` must yield it once.
        use lunaris_core::keyspace::{chunk_key, episode_key};
        use ulid::Ulid;
        let s = mem().await;
        let scope = Scope::new("only-scope").unwrap();
        let id1 = Ulid::new();
        let id2 = Ulid::new();
        s.atomic_write(
            &scope,
            &[
                WriteOp::KvPut { key: episode_key(&scope, id1), value: b"a".to_vec() },
                WriteOp::KvPut { key: chunk_key(&scope, id2), value: b"b".to_vec() },
            ],
        )
        .await
        .expect("seed");
        let page = s.list_scopes(None, 100, None).await.expect("list");
        assert_eq!(page.scopes.len(), 1, "dedupe must collapse two keys into one scope");
        assert_eq!(page.scopes[0].as_str(), "only-scope");
    }

    #[tokio::test]
    async fn list_scopes_pagination_cursor_continuity() {
        let s = mem().await;
        seed_three_scopes(&s).await;
        // Page 1: limit=1 → ["alpha"], cursor=Some("alpha")
        let p1 = s.list_scopes(None, 1, None).await.expect("page 1");
        assert_eq!(p1.scopes.len(), 1);
        assert_eq!(p1.scopes[0].as_str(), "alpha");
        assert_eq!(p1.next_cursor.as_deref(), Some("alpha"));

        // Page 2: cursor="alpha" → ["beta.team"], cursor=Some("beta.team")
        let p2 = s.list_scopes(None, 1, p1.next_cursor.as_deref()).await.expect("page 2");
        assert_eq!(p2.scopes.len(), 1);
        assert_eq!(p2.scopes[0].as_str(), "beta.team");
        assert_eq!(p2.next_cursor.as_deref(), Some("beta.team"));

        // Page 3: cursor="beta.team" → ["gamma_42"], cursor=None (last page).
        let p3 = s.list_scopes(None, 1, p2.next_cursor.as_deref()).await.expect("page 3");
        assert_eq!(p3.scopes.len(), 1);
        assert_eq!(p3.scopes[0].as_str(), "gamma_42");
        assert!(p3.next_cursor.is_none(), "final page must clear cursor");

        // Idempotent past-the-end: explicitly passing the last scope as the
        // cursor MUST return an empty page (cursor semantics: "resume strictly
        // greater than this scope"). This is the safety check for callers
        // that retry with a stale cursor — they must see zero rows, not a
        // restart-from-beginning surprise.
        let p_past = s.list_scopes(None, 1, Some("gamma_42")).await.expect("past-end");
        assert!(p_past.scopes.is_empty(), "cursor past largest scope must return empty page");
        assert!(p_past.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_scopes_prefix_filter() {
        let s = mem().await;
        seed_three_scopes(&s).await;
        let page = s.list_scopes(Some("be"), 100, None).await.expect("filter");
        assert_eq!(page.scopes.len(), 1);
        assert_eq!(page.scopes[0].as_str(), "beta.team");

        // Empty match.
        let none = s.list_scopes(Some("zzz"), 100, None).await.expect("empty");
        assert!(none.scopes.is_empty());
        assert!(none.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_scopes_limit_zero_returns_empty() {
        let s = mem().await;
        seed_three_scopes(&s).await;
        let page = s.list_scopes(None, 0, None).await.expect("zero");
        assert!(page.scopes.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_scopes_skips_non_lunaris_keys() {
        // External KvPut with a non-Lunaris key format MUST be ignored — not
        // surfaced as a phantom scope.
        let s = mem().await;
        s.atomic_write(
            &Scope::dev(),
            &[WriteOp::KvPut { key: b"arbitrary-app-key".to_vec(), value: b"x".to_vec() }],
        )
        .await
        .expect("seed");
        let page = s.list_scopes(None, 100, None).await.expect("list");
        assert!(
            page.scopes.is_empty(),
            "non-Lunaris keys must not surface as scopes; got {:?}",
            page.scopes
        );
    }

    #[tokio::test]
    async fn list_scopes_omits_deleted_scopes() {
        use lunaris_core::keyspace::episode_key;
        use ulid::Ulid;
        let s = mem().await;
        let scope = Scope::new("ephemeral").unwrap();
        let id = Ulid::new();
        let key = episode_key(&scope, id);
        s.atomic_write(&scope, &[WriteOp::KvPut { key: key.clone(), value: b"v".to_vec() }])
            .await
            .expect("put");
        s.atomic_write(&scope, &[WriteOp::KvDelete { key }]).await.expect("delete");
        let page = s.list_scopes(None, 100, None).await.expect("list");
        assert!(
            page.scopes.is_empty(),
            "scope with no live keys must not appear in list_scopes; got {:?}",
            page.scopes
        );
    }
}
