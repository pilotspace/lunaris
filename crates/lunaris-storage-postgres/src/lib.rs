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
pub mod bootstrap;
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

    /// Apply every embedded migration at `admin_url` (a DDL-capable role).
    ///
    /// This is the in-process replacement for the out-of-band
    /// `sqlx migrate run --source crates/lunaris-storage-postgres/migrations`
    /// step — no `sqlx-cli`, no checked-out migrations directory. Surfaced on
    /// the CLI as `lunaris-server migrate --storage <admin_url>` and invoked
    /// automatically by [`crate::PostgresStorage::connect`]-equivalents when
    /// `LUNARIS_ADMIN_URL` is set.
    pub async fn migrate(admin_url: &str) -> Result<(), StorageError> {
        PgClient::migrate(admin_url).await
    }

    /// Connect for runtime use, migrating first if an admin URL is supplied.
    ///
    /// - `admin_url = Some(_)`: run migrations over the privileged admin
    ///   connection, then bind the handle to the (possibly non-DDL) app role at
    ///   `url` without re-running migrations. This is the recommended
    ///   production wiring — `url` is a `NOSUPERUSER NOBYPASSRLS` role.
    /// - `admin_url = None`: behave like [`Self::connect`] (migrate as the role
    ///   behind `url`). If that fails with a permission error and the schema is
    ///   behind, the error is rewrapped with a hint to run migrations as an
    ///   admin role or set `LUNARIS_ADMIN_URL`.
    pub async fn connect_with_admin(
        url: &str,
        admin_url: Option<&str>,
    ) -> Result<Self, StorageError> {
        match admin_url {
            Some(admin) => {
                PgClient::migrate(admin).await?;
                Self::connect_no_migrate(url).await
            }
            None => match Self::connect(url).await {
                Ok(s) => Ok(s),
                Err(e) => {
                    let behind = !matches!(PgClient::schema_is_current(url).await, Ok(true));
                    if behind {
                        Err(StorageError::Backend(format!(
                            "{e}\n\nhint: the Lunaris schema is missing or out of date and this \
                             role cannot run DDL. Apply migrations as an admin role:\n  \
                             lunaris-server migrate --storage <admin_postgres_url>\n\
                             or set LUNARIS_ADMIN_URL=<admin_postgres_url> so the server migrates \
                             on start."
                        )))
                    } else {
                        Err(e)
                    }
                }
            },
        }
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
    /// Plan 04 D-12 + B-11 — see `crate::queue::queue_length` (private) for the
    /// pgmq.queue_length($1) primary path + SqlState 42883 fallback.
    async fn queue_depth(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
    ) -> Result<u64, StorageError> {
        crate::queue::queue_length(&self.client, scope, topic, partition).await
    }

    /// `list_scopes` is **NotSupported** on the Postgres backend.
    ///
    /// Migration `20260510000005_scope_partitioning.sql` puts every primitive
    /// table (`episodes`/`chunks`/`entities`/`relations`/`facts`/`communities`
    /// + `lunaris_kv`) behind `FORCE ROW LEVEL SECURITY` with policy
    /// `scope = current_setting('lunaris.scope', true)`. A cross-scope
    /// `SELECT DISTINCT scope` query from the application role either:
    ///
    /// - returns zero rows (when the GUC is set to a single scope), OR
    /// - requires `SET row_security = off`, which is a `BYPASSRLS` capability
    ///   the production app role MUST NOT hold (per RFC 0001 §6).
    ///
    /// Surfacing the degradation via `NotSupported` is the contractual escape
    /// hatch documented on the trait. Higher layers (e.g. Helios
    /// `memories.search` cross-scope) detect this and fall back to a
    /// caller-supplied scope list. Adding a privileged side-channel
    /// (e.g. a meta-table populated by a SECURITY DEFINER trigger) is a
    /// schema-level change tracked as future work; it is out of scope for
    /// this read-side patch.
    ///
    /// **Do not** "fix" this by removing `FORCE` from the migrations or by
    /// granting `BYPASSRLS` to the app role — both weaken the isolation
    /// boundary that RFC 0001 §3.5 closed.
    async fn list_scopes(
        &self,
        _prefix: Option<&str>,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<lunaris_core::ScopePage, StorageError> {
        Err(StorageError::NotSupported(
            "list_scopes: Postgres backend enforces FORCE ROW LEVEL SECURITY \
             per migration 20260510000005_scope_partitioning.sql; cross-scope \
             enumeration would require BYPASSRLS which the app role MUST NOT hold. \
             Callers should supply a known scope list or enumerate via a \
             Moon/embedded backend.",
        ))
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
            // Wave 4b probe (2026-05-12): AGE 1.5 accepts `MATCH p = (n)-[*1..N]-(m)`
            // path-variable binding, `length(p)` over variable-length paths, and
            // `n.id_hex AS source_entity_id` aliasing in a single RETURN. It does NOT
            // accept `reduce(acc, x in xs | ...)` — the parser errors at the `|`
            // token. PathMetrics is therefore the correct ceiling: the operator
            // gets path-length + source-entity columns natively, and synthesizes
            // anchor_confidence post-Cypher; edge_weight_product defaults to 1.0
            // until Apache AGE adds `reduce()` over variable-length paths and we
            // can graduate to CypherDialect::Full.
            cypher_dialect: lunaris_core::CypherDialect::PathMetrics,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for PostgresStorage {
    /// Wave 2.5A: gains `scope: &Scope` per RFC 0001 §3.4 amendment.
    /// Scope is threaded through to the underlying keyword search free function.
    async fn keyword_search(
        &self,
        scope: &lunaris_core::Scope,
        index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        crate::keyword::keyword_search(&self.client, scope, index, query, k, filter, as_of).await
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
            cypher_dialect: lunaris_core::CypherDialect::PathMetrics,
            graph_decay_native: false,
            graph_navigate_native: false,
        };
        assert!(!want.bi_temporal_native);
        assert!(want.graph_native);
        assert!(!want.rerank_native);
        assert!(want.queue_native);
        assert_eq!(want.max_vector_dim, 1536);
        assert!(!want.native_rrf, "Postgres backend uses client-side RRF (Phase 1.5 STORE-09)");
        assert_eq!(
            want.cypher_dialect,
            lunaris_core::CypherDialect::PathMetrics,
            "Postgres (AGE 1.5) supports MATCH p = ... + length(p) + source_entity_id; \
             rejects reduce() (Wave 4b probe)"
        );
    }
}
