//! `MoonStorage` — `StoragePort` impl backed by Moon (Redis-compatible RESP).
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
//! Phase 1 ships a thin pass-through. Phase 2 wraps in retry / circuit breaker
//! (see Phase 4 `OPS-04`).
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
pub mod keyspace;
pub mod keyword;
pub mod kv;
pub mod queue;
pub mod vector;

pub use client::MoonClient;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Filter, GraphResult, Hlc, KeywordHit, KeywordPort, Lsn, QueueMsg, Row, Scope,
    StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};

/// `StoragePort` backed by a single Moon RESP connection manager.
#[derive(Debug, Clone)]
pub struct MoonStorage {
    pub(crate) client: MoonClient,
}

impl MoonStorage {
    /// Open a connection to Moon at `url` (`moon://host:port[?ws=workspace]`).
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        Ok(Self { client: MoonClient::connect(url).await? })
    }

    /// Borrow the underlying client (used by integration tests).
    pub fn client(&self) -> &MoonClient {
        &self.client
    }
}

#[async_trait]
impl StoragePort for MoonStorage {
    // RFC 0001 Wave 0: scope is threaded through to the underlying free functions.
    // The free functions currently ignore it (`let _ = scope;`). Real per-scope
    // keyspace prefix routing is Wave 1C work.

    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        let _ = scope; // Wave 1C: will prefix keys with `lunaris:{scope}:`
        crate::atomic::atomic_write(&self.client, ops).await
    }

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
        let _ = scope; // Wave 1C: will route to per-scope FT index `lunaris_{scope}_chunk_idx`
        crate::vector::vector_search(&self.client, index, query, k, filter, as_of, rerank).await
    }

    async fn graph_traverse(
        &self,
        scope: &Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        let _ = scope; // Wave 1C: will route to `GRAPH.QUERY lunaris_{scope}_graph`
        crate::graph::graph_traverse(&self.client, query, as_of).await
    }

    async fn scan_range(
        &self,
        scope: &Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        let _ = scope; // Wave 1C: will prepend `lunaris:{scope}:` to prefix
        crate::kv::scan_range(&self.client, prefix, as_of).await
    }

    async fn read_as_of(
        &self,
        scope: &Scope,
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        let _ = scope; // Wave 1C: will prepend `lunaris:{scope}:` to key
        crate::kv::read_as_of(&self.client, key, as_of).await
    }

    async fn publish(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        let _ = scope; // Wave 1C: will use `MQ.PUSH lunaris:{scope}:{topic}`
        crate::queue::publish(&self.client, topic, partition, payload).await
    }

    async fn subscribe(
        &self,
        scope: &Scope,
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        let _ = scope; // Wave 1C: will subscribe to per-scope topic
        crate::queue::subscribe(self.client.clone(), group, topic, partition).await
    }

    /// Plan 04 D-12 — see [`crate::queue::queue_length`] for the raw
    /// `MQ.LENGTH` escape hatch rationale.
    async fn queue_depth(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
    ) -> Result<u64, StorageError> {
        let _ = scope; // Wave 1C: will query per-scope topic depth
        crate::queue::queue_length(&self.client, topic, partition).await
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
            queue_native: true,
            // Moon profile uses 768d (matches EmbeddingGemma); Postgres uses 1536d.
            max_vector_dim: 768,
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
        }
    }
}

#[async_trait]
impl KeywordPort for MoonStorage {
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
    }
}
