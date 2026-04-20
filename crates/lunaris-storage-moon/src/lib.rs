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
    CypherQuery, Filter, GraphResult, Hlc, KeywordHit, KeywordPort, Lsn, QueueMsg, Row,
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
    async fn atomic_write(&self, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        crate::atomic::atomic_write(&self.client, ops).await
    }

    async fn vector_search(
        &self,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        crate::vector::vector_search(&self.client, index, query, k, filter, as_of, rerank).await
    }

    async fn graph_traverse(
        &self,
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        crate::graph::graph_traverse(&self.client, query, as_of).await
    }

    async fn scan_range(
        &self,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        crate::kv::scan_range(&self.client, prefix, as_of).await
    }

    async fn read_as_of(&self, key: &[u8], as_of: Hlc) -> Result<Option<Row<Bytes>>, StorageError> {
        crate::kv::read_as_of(&self.client, key, as_of).await
    }

    async fn publish(
        &self,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        crate::queue::publish(&self.client, topic, partition, payload).await
    }

    async fn subscribe(
        &self,
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        crate::queue::subscribe(self.client.clone(), group, topic, partition).await
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: true,
            rerank_native: true,
            queue_native: true,
            // Moon profile uses 768d (matches EmbeddingGemma); Postgres uses 1536d.
            max_vector_dim: 768,
            // Moon's `text().hybrid_search()` runs `FT.SEARCH HYBRID VECTOR ... SPARSE
            // ... FUSION RRF` natively in one round trip — Phase 2's `fuse_rrf` opts
            // into `RrfFusion::Moon` when this is true (Phase 1.5 STORE-09).
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
            bi_temporal_native: true,
            graph_native: true,
            rerank_native: true,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: true,
        };
        assert!(want.bi_temporal_native);
        assert!(want.graph_native);
        assert!(want.rerank_native);
        assert!(want.queue_native);
        assert_eq!(want.max_vector_dim, 768);
        assert!(
            want.native_rrf,
            "Moon backend supports text().hybrid_search RRF (Phase 1.5 STORE-09)"
        );
    }
}
