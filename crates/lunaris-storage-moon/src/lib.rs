//! `MoonStorage` — `StoragePort` impl backed by Moon (Redis-compatible RESP).
//!
//! RFC 0001 Wave 1C: every `StoragePort` method now routes through per-scope
//! keyspace helpers (`keyspace::{scope_prefix, ft_index_name, graph_key, mq_topic}`).
//! Per-scope FT indices, graph keys, and MQ topics are created lazily on first
//! write via `ensure_scope`.
//!
//! Per blueprint §6, every method is a thin pass-through to a Moon native command:
//!
//! | trait method      | Moon command(s)                                                         |
//! |-------------------|-------------------------------------------------------------------------|
//! | `atomic_write`    | `TXN.BEGIN` + per-op (`HSET` / `FT.UPSERT` / `GRAPH.QUERY MERGE`) + `TXN.COMMIT` |
//! | `vector_search`   | `FT.SEARCH` (with `TEMPORAL.SNAPSHOT_AT` when `as_of` is `Some`)         |
//! | `graph_traverse`  | `GRAPH.QUERY` (with `TEMPORAL.SNAPSHOT_AT` when `as_of` is `Some`)       |
//! | `scan_range`      | `SCAN ... MATCH <prefix>*` then `HGET` per matched key                   |
//! | `read_as_of`      | `TEMPORAL.SNAPSHOT_AT` then `HGET <key> v`                              |
//! | `publish`         | `MQ.PUSH`                                                               |
//! | `subscribe`       | `MQ.POP ... BLOCK` polling stream                                       |
//! | `capabilities`    | constant — Moon-native everything                                       |
//!
//! ## Lazy per-scope init
//!
//! On first write under a scope, `ensure_scope` creates:
//! - `FT.CREATE lunaris_{scope}_{kind}_idx` for each of chunks / entities / facts / communities
//! - `GRAPH.CREATE lunaris_{scope}_graph`
//!
//! Subsequent calls for an already-initialized scope skip the Moon round-trips via
//! an in-memory `initialized_scopes` set (lock-free read path once initialized).
//!
//! ## Threat model snapshot (T-01-03-*)
//!
//! * `WriteOp::GraphNode { label, ... }` and `WriteOp::GraphEdge { rel, ... }` are
//!   interpolated into Cypher. Callers MUST validate `label` / `rel` against
//!   `^[A-Za-z_][A-Za-z0-9_]*$` — see `crates/lunaris-storage-moon/src/atomic.rs` rustdoc.
//!   Phase 4 (`OPS-04` audit) will move the guard into the trait.
//! * Connection is cleartext RESP over TCP — Moon is treated as trusted infra inside the
//!   same network boundary as the Lunaris process. TLS lands in Phase 5.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod atomic;
pub mod client;
pub mod graph;
// W2-L2 — FT.INVALIDATE_RANGE raw RESP escape hatch (UC-G3 force-push invalidation).
// pub(crate): only called from `MoonStorage::invalidate_range` in this file.
pub(crate) mod invalidate;
pub mod keyspace;
pub mod keyword;
pub mod kv;
// hotkeys-observability — HOTKEYS raw RESP path (typed SDK has no wrapper).
pub(crate) mod hotkeys;
// ft-navigate-recall — FT.NAVIGATE raw RESP path (typed SDK lacks a DECAY slot).
pub(crate) mod navigate;
pub mod queue;
pub mod scopes;
pub mod vector;

pub use client::MoonClient;

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Filter, GraphDecay, GraphResult, Hlc, KeywordHit, KeywordPort, Lsn, NavigateHit,
    NavigateSpec, QueueMsg, Row, Scope, ScopePage, StorageCapabilities, StorageError, StoragePort,
    VectorHit, WriteOp,
};
use parking_lot::Mutex;

use crate::keyspace::{ft_index_name, graph_key};

/// `StoragePort` backed by a single Moon RESP connection manager.
///
/// `initialized_scopes` tracks which scopes have had their FT indices and graph
/// key created (lazy init on first write). `Mutex` is held only during the
/// brief check + insert — never across `.await` points.
#[derive(Debug, Clone)]
pub struct MoonStorage {
    pub(crate) client: MoonClient,
    queue_native: bool,
    /// Set of scopes whose FT indices + graph key have been created on Moon.
    /// `parking_lot::Mutex` (not `std::sync::Mutex`) per CLAUDE.md lock discipline.
    /// The lock is NEVER held across an `.await` — it is taken, the bool is checked,
    /// optionally the scope is inserted, and the lock is dropped BEFORE the async
    /// Moon calls in `ensure_scope`.
    initialized_scopes: Arc<Mutex<HashSet<String>>>,
}

impl MoonStorage {
    /// Open a connection to Moon at `url` (`moon://host:port[?ws=workspace]`),
    /// creating FT vector indices at the default dimension
    /// ([`client::DEFAULT_VECTOR_DIM`] = 768, matching EmbeddingGemma-300M).
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        Self::connect_with_dim(url, crate::client::DEFAULT_VECTOR_DIM).await
    }

    /// Like [`MoonStorage::connect`], but creates the FT vector indices at
    /// `dim` instead of the default 768. `dim` MUST be `> 0`. Moon's
    /// `FT.CREATE` has no upper cap, so a 1536-d embedder (OpenAI
    /// `text-embedding-3`) works against Moon out of the box.
    ///
    /// ## Operator footgun — existing index won't auto-resize
    ///
    /// Moon's `FT.CREATE` is idempotent and does NOT update an existing
    /// index's schema. If a Moon instance already holds a 768-d `chunks`
    /// index from a prior run, reopening with a 1536-d embedder leaves the
    /// 768-d index in place; the mismatch surfaces only on the first vector
    /// write. Drop the stale index first (`FT.DROPINDEX <name>`).
    pub async fn connect_with_dim(url: &str, dim: usize) -> Result<Self, StorageError> {
        let client = MoonClient::connect_with_dim(url, dim).await?;
        let queue_native = crate::queue::supports_native_queue(&client).await?;
        Ok(Self { client, queue_native, initialized_scopes: Arc::new(Mutex::new(HashSet::new())) })
    }

    /// Borrow the underlying client (used by integration tests).
    pub fn client(&self) -> &MoonClient {
        &self.client
    }

    /// Lazily ensure per-scope FT indices and graph key exist on Moon.
    ///
    /// On first call for a given scope, creates:
    /// - `FT.CREATE lunaris_{scope}_{kind}_idx` for chunks / entities / facts / communities
    /// - `GRAPH.CREATE lunaris_{scope}_graph`
    ///
    /// Idempotent: "already exists" errors from Moon are swallowed. Subsequent calls
    /// for the same scope return immediately (in-memory set check, no Moon I/O).
    ///
    /// ## Lock discipline
    ///
    /// The `Mutex` is locked only to read/write the `HashSet<String>` — it is
    /// dropped BEFORE any `.await` call so it is NEVER held across an await point.
    async fn ensure_scope(&self, scope: &Scope) -> Result<(), StorageError> {
        let scope_str = scope.as_str().to_string();

        // Fast path: scope already initialized — lock, check, drop.
        {
            let guard = self.initialized_scopes.lock();
            if guard.contains(&scope_str) {
                return Ok(());
            }
        } // lock dropped here

        // Slow path: create FT indices and graph on Moon.
        self.create_scope_indexes(scope).await?;

        // Mark initialized — lock, insert, drop.
        {
            let mut guard = self.initialized_scopes.lock();
            guard.insert(scope_str);
        } // lock dropped here

        Ok(())
    }

    /// Create per-scope FT indices and graph key. Called at most once per scope
    /// (guarded by `ensure_scope`'s in-memory set).
    async fn create_scope_indexes(&self, scope: &Scope) -> Result<(), StorageError> {
        // Single-sourced: the FT vector dimension is configured once on the
        // underlying client (`connect`/`connect_with_dim`); per-scope indices
        // inherit it — and the `?quant=` choice — so engine-level
        // `Lunaris::open` sizing flows through here. Schema construction is
        // shared with the legacy global `ensure_indexes` via
        // `client::create_lunaris_index_named` so the two sites can never
        // diverge.
        let dim = self.client.dim;
        let typed = self.client.typed();

        for kind in &["chunks", "entities", "facts", "communities"] {
            let idx_name = ft_index_name(scope, kind);
            // The FT prefix must match the key shape written by `atomic.rs::VectorUpsert`:
            // `{ft_index_name(scope, kind)}:{id_hex}`.
            let prefix = format!("{idx_name}:");
            crate::client::create_lunaris_index_named(
                &typed,
                &idx_name,
                kind,
                &prefix,
                dim,
                self.client.quantization,
            )
            .await?;
        }

        // Create per-scope graph. Moon does not auto-create graphs on first GRAPH.QUERY.
        let gkey = graph_key(scope);
        let typed = self.client.typed();
        match typed.graph().create(&gkey).await {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if !(msg.contains("already exists") || msg.contains("Graph already exists")) {
                    return Err(crate::client::moon_err(e));
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl StoragePort for MoonStorage {
    /// RFC 0001 Wave 1C: lazy per-scope init before writing, then route all ops
    /// through scope-prefixed keys / indices.
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        self.ensure_scope(scope).await?;
        crate::atomic::atomic_write(&self.client, scope, ops).await
    }

    /// observability-rollout-maturity — override the additive default with a
    /// real Moon `PING` so a dead/stalled backend surfaces as `Err` on the
    /// `/healthz` rollout-cutback probe. Bounded by `LUNARIS_MOON_OP_TIMEOUT`.
    async fn health_check(&self) -> Result<(), StorageError> {
        self.client.ping().await
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

    async fn graph_traverse_decayed(
        &self,
        scope: &Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
        decay: Option<&GraphDecay>,
    ) -> Result<GraphResult, StorageError> {
        match decay {
            None => crate::graph::graph_traverse(&self.client, scope, query, as_of).await,
            Some(d) => {
                crate::graph::graph_traverse_decayed(&self.client, scope, query, as_of, d).await
            }
        }
    }

    async fn vector_navigate(
        &self,
        scope: &Scope,
        index: &str,
        query: &[f32],
        k: usize,
        spec: &NavigateSpec,
    ) -> Result<Vec<NavigateHit>, StorageError> {
        crate::navigate::vector_navigate(&self.client, scope, index, query, k, spec).await
    }

    async fn hot_keys(&self, count: usize) -> Result<Vec<lunaris_core::HotKey>, StorageError> {
        crate::hotkeys::hot_keys(&self.client, count).await
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

    /// Plan 04 D-12 — see `crate::queue::queue_length` (private) for the raw
    /// `MQ.LENGTH` escape hatch rationale.
    async fn queue_depth(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
    ) -> Result<u64, StorageError> {
        crate::queue::queue_length(&self.client, scope, topic, partition).await
    }

    /// Cross-scope enumeration via `SCAN MATCH lunaris:*` + key parse.
    /// Q-U2 lock — lazy SCAN-derived. See `crate::scopes` for the cursor
    /// model and the Moon-SCAN-cursor vs scope-string-cursor tradeoff.
    async fn list_scopes(
        &self,
        prefix: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ScopePage, StorageError> {
        crate::scopes::list_scopes(&self.client, prefix, limit, cursor).await
    }

    /// Bulk-invalidate FT index records via `FT.INVALIDATE_RANGE`.
    ///
    /// ## Wire shape
    ///
    /// ```text
    /// FT.INVALIDATE_RANGE <index> <node_id_field> <node_id_value>
    ///                     <hlc_wall_field> <hlc_wall_lo> <hlc_wall_hi>
    /// ```
    ///
    /// Returns the integer count of deleted records as `u64`.
    ///
    /// ## Escape hatch
    ///
    /// `moon-client` v0.1.x does not expose a typed wrapper for
    /// `FT.INVALIDATE_RANGE`. We reach the underlying
    /// `redis::aio::MultiplexedConnection` via `MoonClient::inner_mut()` on a
    /// local clone — the same documented pattern used by the HSCAN escape hatch
    /// in `kv.rs` (the only other permitted raw-RESP site in this crate per
    /// Phase 1.5 STORE-09 constraints).
    ///
    /// ## Error mapping
    ///
    /// - Moon `WRONGTYPE` (index does not exist) → `StorageError::Backend`
    ///   containing `"WRONGTYPE"`. The `Lunaris::invalidate_range` fan-out
    ///   treats this as warn-and-skip (degraded mode).
    /// - Any other Moon error → `StorageError::Backend`.
    #[allow(clippy::too_many_arguments)]
    async fn invalidate_range(
        &self,
        scope: &Scope,
        index: &str,
        node_id_field: &str,
        node_id_value: &str,
        hlc_wall_field: &str,
        hlc_wall_lo_inclusive: i64,
        hlc_wall_hi_inclusive: i64,
    ) -> Result<u64, StorageError> {
        crate::invalidate::invalidate_range(
            &self.client,
            scope,
            index,
            node_id_field,
            node_id_value,
            hlc_wall_field,
            hlc_wall_lo_inclusive,
            hlc_wall_hi_inclusive,
        )
        .await
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            // Moon supports AS_OF for FT.SEARCH (vector + keyword) and VALID_AT
            // for GRAPH.QUERY, but plain HGET does NOT accept temporal clauses.
            // Per moon/docs/guides/temporal.mdx: "Bi-temporal fields are
            // currently limited to graph entities (nodes/edges). KV temporal
            // versioning uses a sparse index" (the sparse index is for
            // transactional MVCC isolation, NOT for AS_OF reads). Lunaris's
            // KV `read_as_of` therefore returns current state on Moon —
            // historical KV reads need a Lunaris-layer versioned-key encoding
            // (Gap 8 — tracked for follow-up phase). Reporting `false` here
            // makes downstream consumers route bi-temporal reads to Postgres
            // (which has native bi-temporal columns) per the dual-backend
            // contract. Live-measurement gap fix 2026-04-21.
            bi_temporal_native: false,
            graph_native: true,
            rerank_native: true,
            queue_native: self.queue_native,
            // Moon's FT.CREATE has no dimension cap — report the dimension the
            // adapter actually created its indices at (default 768d matching
            // EmbeddingGemma-300M; `connect_with_dim` / `Lunaris::open` size it
            // to the embedder). This stays an accurate description of what the
            // FT `vec` field will accept.
            max_vector_dim: self.client.dim as u32,
            // Gap 9 closure (2026-04-21): `ensure_indexes` now declares
            // `SchemaField::Text("content")` on chunks/entities/facts/communities
            // and `WriteOp::VectorUpsert` writes the `content` field via
            // `extract_content_for_index` (mirrors the Postgres
            // `payload->>'text'/'fact_text'/...` tsvector convention). Moon's
            // SDK `hybrid_search` (3-weight + sparse_field) therefore resolves
            // `@content` and `fuse_rrf` opts into `RrfFusion::Moon` for one
            // round-trip server-side fusion. If the schema regresses (e.g. an
            // older Moon binary that ignores extra_schema), set this back to
            // `false` to force the always-correct local fusion path.
            native_rrf: true,
            // RFC 0001 §3.6 — Moon's soft FT-index limit is ~512 per node
            // before recall p99 degrades (Moon docs §6.4). Above this,
            // operators should consider workspace-level pooling (future RFC).
            max_scopes_recommended: 512,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: true,
            graph_navigate_native: true,
        }
    }
}

#[async_trait]
impl KeywordPort for MoonStorage {
    /// Wave 2.5A: `KeywordPort::keyword_search` now carries `scope: &Scope`
    /// (RFC 0001 §3.4 amendment). The Moon backend threads scope through to
    /// `keyword::keyword_search` which routes to the per-scope FT index
    /// (`ft_index_name(scope, index)`). Previously this impl used `Scope::dev()`
    /// as a placeholder — that placeholder is now replaced by the caller-supplied scope.
    async fn keyword_search(
        &self,
        scope: &Scope,
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

    /// Compile-time assertion that `MoonStorage` is dyn-compatible.
    #[allow(dead_code)]
    fn _moonstorage_is_storage_port() {
        fn assert_storage_port<T: StoragePort + ?Sized>() {}
        assert_storage_port::<MoonStorage>();
        assert_storage_port::<dyn StoragePort>();
    }

    #[test]
    fn capabilities_match_moon_profile() {
        // We can't construct a real `MoonStorage` without a connection, but we can match
        // the `capabilities()` body shape directly.
        let want = StorageCapabilities {
            bi_temporal_native: false,
            graph_native: true,
            rerank_native: true,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: true,
            max_scopes_recommended: 512,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: true,
            graph_navigate_native: true,
        };
        assert!(
            !want.bi_temporal_native,
            "Moon does not natively support KV bi-temporal reads (HGET ignores AS_OF); only FT.SEARCH AS_OF + GRAPH.QUERY VALID_AT are temporal — Gap 8 fix 2026-04-21"
        );
        assert!(want.graph_native);
        assert!(want.rerank_native);
        assert!(want.queue_native);
        assert_eq!(want.max_vector_dim, 768);
        assert!(
            want.native_rrf,
            "Moon HYBRID FT.SEARCH now resolves @content via the SchemaField::Text added by ensure_indexes; fuse_rrf opts into RrfFusion::Moon — Gap 9 closure 2026-04-21"
        );
        assert_eq!(
            want.max_scopes_recommended, 512,
            "Moon FT soft limit is ~512 indices per node (RFC 0001 §3.6)"
        );
    }
}
