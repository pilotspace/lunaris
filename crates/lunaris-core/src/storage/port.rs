//! `StoragePort` — the only abstraction in Lunaris with strict guarantees.
//!
//! Trait shape per blueprint §6 with two object-safety deviations documented on the trait:
//!   1. `scan_range` and `read_as_of` accept `&[u8]` instead of a generic `K: Key` parameter.
//!      Object-safety (`Arc<dyn StoragePort>`) is a hard requirement of `STORE-08`'s URL
//!      router. Callers with typed keys call `.as_bytes()` at the call site (the `Key`
//!      trait is still defined and exported in `super::types`).
//!   2. Stream items are wrapped in `Result<_, StorageError>` so mid-stream backend failures
//!      surface per-row instead of silently truncating.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::error::StorageError;
use crate::hlc::Hlc;

use super::capabilities::StorageCapabilities;
use super::types::{CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp};

#[async_trait]
pub trait StoragePort: Send + Sync + 'static {
    /// Atomic multi-key write. Either all ops commit or none do.
    /// Returns the `Lsn` at which the writes became visible.
    async fn atomic_write(&self, ops: &[WriteOp]) -> Result<Lsn, StorageError>;

    /// Vector search with structured filter. `rerank=true` is a HINT — backends without
    /// native rerank should return `rerank_applied=false` in each hit; the retriever
    /// may then apply rerank downstream or skip per `degraded_fallback`.
    async fn vector_search(
        &self,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError>;

    /// Graph traversal via Cypher. Backends without native graph return
    /// `Err(StorageError::NotSupported(_))`; retrievers degrade to vector-only.
    async fn graph_traverse(
        &self,
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError>;

    /// KV scan by prefix with optional bi-temporal snapshot.
    ///
    /// **Deviation 1 from blueprint §6:** the blueprint signature is
    /// `scan_range<K: Key>(...)` — generic methods are not object-safe in Rust, so this
    /// method takes `&[u8]` and callers with typed keys call `.as_bytes()` at the call
    /// site. The `Key` trait is still defined in `super::types`.
    ///
    /// **Deviation 2 from blueprint §6:** the stream item is `Result<(Bytes, Bytes),
    /// StorageError>` instead of `(Bytes, Bytes)` so mid-stream failures (network
    /// drops, snapshot expiry) surface per-row instead of silently truncating.
    async fn scan_range(
        &self,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError>;

    /// MVCC snapshot read of a single key. Returns `None` if the key does not exist
    /// at `as_of`.
    ///
    /// **Deviation 1 from blueprint §6:** the blueprint signature is
    /// `read_as_of<K: Key>(...)` — see the `scan_range` doc-comment for why this method
    /// takes `&[u8]`.
    async fn read_as_of(&self, key: &[u8], as_of: Hlc) -> Result<Option<Row<Bytes>>, StorageError>;

    /// Queue publish. Returns the offset assigned by the broker (monotonic within
    /// `(topic, partition)`).
    async fn publish(
        &self,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError>;

    /// Queue subscribe. Stream emits `Result<QueueMsg, StorageError>` per message;
    /// stream is `'static` because subscriptions outlive any single request frame.
    ///
    /// **Deviation 2 from blueprint §6:** stream items are `Result`-wrapped so
    /// mid-stream broker failures surface per-message.
    async fn subscribe(
        &self,
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError>;

    /// Plan 04 D-12 — queue introspection. Returns the number of pending
    /// (un-ACKed) messages on `(topic, partition)`.
    ///
    /// **Additive trait method**: backends may opt in by overriding the
    /// default; the default returns `Err(StorageError::NotSupported(...))` so
    /// every existing impl keeps compiling without modification. The Moon
    /// backend implements this via raw `redis::cmd("MQ.LENGTH")` (Path 2 from
    /// the D-12 B-NOTE — moon-client v0.1.x lacks a typed wrapper); the
    /// Postgres backend implements it via `pgmq.queue_length($1)` with a
    /// SqlState 42883 fallback for older pgmq installs (B-11).
    ///
    /// Plan 04-04 `Lunaris::recall_with_degraded_check` reads this once per
    /// recall to set `Hit::degraded = true` when the verifier queue depth
    /// crosses `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` (VERIFY-05 + VERIFY-06).
    async fn queue_depth(
        &self,
        _topic: &str,
        _partition: u16,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported(
            "queue_depth not implemented for this StoragePort backend",
        ))
    }

    /// Report capabilities so higher layers (retrievers, recipes, the conformance
    /// suite) can make degradation decisions per blueprint §6.
    fn capabilities(&self) -> StorageCapabilities;
}
