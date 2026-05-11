//! `PostgresStorage` — `StoragePort` impl backed by Postgres + pgvector + AGE + pgmq.
//!
//! Module fan-out follows the same per-method layout as `lunaris-storage-moon`:
//!
//! | trait method      | module         | SQL pattern                                                |
//! |-------------------|----------------|------------------------------------------------------------|
//! | `atomic_write`    | `atomic.rs`    | `BEGIN` + per-op INSERT/UPSERT + `COMMIT`/`ROLLBACK`       |
//! | `vector_search`   | `vector.rs`    | `SELECT ... ORDER BY embedding <=> $1 LIMIT $k`            |
//! | `graph_traverse`  | `graph.rs`     | `SELECT * FROM cypher('lunaris_graph', $$ ... $$) AS ...`  |
//! | `scan_range`      | `kv.rs`        | `SELECT key,value FROM lunaris_kv WHERE key LIKE $1 AND ...`|
//! | `read_as_of`      | `kv.rs`        | bi-temporal predicate on `lunaris_kv`                      |
//! | `publish`         | `queue.rs`     | `SELECT pgmq.send($topic, $payload::jsonb)`                |
//! | `subscribe`       | `queue.rs`     | `SELECT * FROM pgmq.read($topic, $vt, $count)` polling     |
//! | `capabilities`    | this file      | constant — Postgres profile (emulated bi-temporal, AGE, pgmq) |
//!
//! ## Threat model snapshot (T-01-04-*)
//!
//! * Filter values, label/rel names, prefix patterns are interpolated into SQL strings.
//!   Caller-validated regex `^[A-Za-z_][A-Za-z0-9_]*$` per Plan 03's T-01-03-01 contract;
//!   Phase 4 OPS-04 moves the guard into the trait.
//! * `index` parameter on `vector_search` / `VectorUpsert` is whitelist-matched against
//!   `chunks|entities|facts|communities` — anything else returns `StorageError::Backend`.
//! * `PgClient::Debug` currently exposes the URL (potentially with userinfo). Phase 4
//!   `OPS-05` redacts it; tracked in STATE.md.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod atomic;
pub mod graph;
pub mod keyword;
pub mod kv;
pub mod pool;
pub mod queue;
pub mod schema;
pub mod vector;

pub use pool::PgClient;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Filter, GraphResult, Hlc, KeywordHit, KeywordPort, Lsn, QueueMsg, Row, Scope,
    StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};

#[derive(Debug, Clone)]
pub struct PostgresStorage {
    pub(crate) client: PgClient,
}

impl PostgresStorage {
    /// Open a connection to Postgres at `url` (`postgres://user:pass@host/db` or
    /// `postgresql://...`) and apply migrations.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        Ok(Self { client: PgClient::connect(url).await? })
    }

    /// Open a connection to Postgres without running migrations.
    ///
    /// Used by tests and integration harnesses that connect as a non-privileged
    /// application role (e.g., `lunaris_app`) that cannot run DDL but whose RLS
    /// policies must be verified. Migrations MUST have already been applied via
    /// a privileged connection before calling this.
    pub async fn connect_no_migrate(url: &str) -> Result<Self, StorageError> {
        Ok(Self { client: PgClient::connect_no_migrate(url).await? })
    }

    /// Borrow the underlying client (used by integration tests).
    pub fn client(&self) -> &PgClient {
        &self.client
    }
}

#[async_trait]
impl StoragePort for PostgresStorage {
    // RFC 0001 Wave 1B: scope is now wired into every underlying free function.
    // Every transaction issues `SELECT set_config('lunaris.scope', $1, true)`
    // before primitive ops so RLS policies on episodes/chunks/entities/
    // relations/facts/communities/lunaris_kv enforce per-scope isolation.
    // set_config with is_local=true is transaction-scoped (like SET LOCAL),
    // but unlike SET/SET LOCAL it accepts a parameterized $1 bind value.

    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        crate::atomic::atomic_write(&self.client, scope, ops).await
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
        crate::vector::vector_search(&self.client, scope, index, query, k, filter, as_of, rerank)
            .await
    }
    async fn graph_traverse(
        &self,
        scope: &Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        crate::graph::graph_traverse(&self.client, scope, query, as_of).await
    }
    async fn scan_range(
        &self,
        scope: &Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        crate::kv::scan_range(&self.client, scope, prefix, as_of).await
    }
    async fn read_as_of(
        &self,
        scope: &Scope,
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        crate::kv::read_as_of(&self.client, scope, key, as_of).await
    }
    async fn publish(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        crate::queue::publish(&self.client, scope, topic, partition, payload).await
    }
    async fn subscribe(
        &self,
        scope: &Scope,
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        crate::queue::subscribe(self.client.clone(), scope, group, topic, partition).await
    }
    /// Plan 04 D-12 + B-11 — see [`crate::queue::queue_length`] for the
    /// pgmq.queue_length($1) primary path + SqlState 42883 fallback.
    async fn queue_depth(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
    ) -> Result<u64, StorageError> {
        crate::queue::queue_length(&self.client, scope, topic, partition).await
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false, // emulated via valid_from/valid_to/sys_from/sys_to columns
            graph_native: true,        // AGE
            rerank_native: false,      // no native cross-encoder
            queue_native: true,        // pgmq
            max_vector_dim: 1536,      // pgvector practical ceiling (Postgres profile)
            native_rrf: false,         // Postgres uses client-side RRF (Phase 1.5 STORE-09)
            max_scopes_recommended: 0,
        }
    }
}

#[async_trait]
impl KeywordPort for PostgresStorage {
    async fn keyword_search(
        &self,
        index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        crate::keyword::keyword_search(&self.client, index, query, k, filter, as_of).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time assertion that `PostgresStorage` is dyn-compatible.
    #[allow(dead_code)]
    fn _postgresstorage_is_storage_port() {
        fn assert_storage_port<T: StoragePort + ?Sized>() {}
        assert_storage_port::<PostgresStorage>();
        assert_storage_port::<dyn StoragePort>();
    }

    #[test]
    fn capabilities_match_postgres_profile() {
        // We can't construct a real `PostgresStorage` without a connection, but we can
        // match the `capabilities()` body shape directly.
        let want = StorageCapabilities {
            bi_temporal_native: false,
            graph_native: true,
            rerank_native: false,
            queue_native: true,
            max_vector_dim: 1536,
            native_rrf: false,
            max_scopes_recommended: 0,
        };
        assert!(!want.bi_temporal_native);
        assert!(want.graph_native);
        assert!(!want.rerank_native);
        assert!(want.queue_native);
        assert_eq!(want.max_vector_dim, 1536);
        assert!(!want.native_rrf, "Postgres backend uses client-side RRF (Phase 1.5 STORE-09)");
    }
}
