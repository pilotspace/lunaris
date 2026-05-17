//! `StoragePort` — the only abstraction in Lunaris with strict guarantees.
//!
//! Trait shape per blueprint §6 with two object-safety deviations documented on the trait:
//!   1. `scan_range` and `read_as_of` accept `&[u8]` instead of a generic `K: Key` parameter.
//!      Object-safety (`Arc<dyn StoragePort>`) is a hard requirement of `STORE-08`'s URL
//!      router. Callers with typed keys call `.as_bytes()` at the call site (the `Key`
//!      trait is still defined and exported in `super::types`).
//!   2. Stream items are wrapped in `Result<_, StorageError>` so mid-stream backend failures
//!      surface per-row instead of silently truncating.
//!
//! RFC 0001 (v0.2): every partitioning method now takes `scope: &Scope` as the first
//! argument after `&self`. `capabilities()` is unchanged. Wave 0 backend impls thread scope
//! through to the underlying free functions; the free functions may `let _ = scope;` —
//! real per-scope partitioning is Wave 1B (Postgres RLS) and Wave 1C (Moon keyspace prefix).

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::error::StorageError;
use crate::hlc::Hlc;
use crate::scope::Scope;

use super::capabilities::StorageCapabilities;
use super::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, ScopePage, VectorHit, WriteOp,
};

/// The storage abstraction for Lunaris.
///
/// See module-level doc for the object-safety contract and RFC 0001 §3.4 for the
/// `&Scope` argument rationale.
#[async_trait]
pub trait StoragePort: Send + Sync + 'static {
    /// Atomic multi-key write. Either all ops commit or none do.
    /// Returns the `Lsn` at which the writes became visible.
    ///
    /// `scope` is authoritative for the whole batch — `WriteOp` variants do NOT carry
    /// individual scopes (RFC 0001 §3.4: a single `atomic_write` is by definition one
    /// scope; cross-scope atomicity is out of scope for v0.2).
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError>;

    /// Vector search with structured filter. `rerank=true` is a HINT — backends without
    /// native rerank should return `rerank_applied=false` in each hit; the retriever
    /// may then apply rerank downstream or skip per `degraded_fallback`.
    ///
    /// The 8-argument signature exceeds clippy's default limit of 7. The arguments
    /// are all semantically distinct and cannot be collapsed without introducing a
    /// `VectorSearchRequest` builder that adds more boilerplate than it saves. The
    /// lint is suppressed here and at every impl site.
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
    ) -> Result<Vec<VectorHit>, StorageError>;

    /// Graph traversal via Cypher. Backends without native graph return
    /// `Err(StorageError::NotSupported(_))`; retrievers degrade to vector-only.
    async fn graph_traverse(
        &self,
        scope: &Scope,
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
        scope: &Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError>;

    /// MVCC snapshot read of a single key. Returns `None` if the key does not exist
    /// at `as_of`.
    ///
    /// **Deviation 1 from blueprint §6:** the blueprint signature is
    /// `read_as_of<K: Key>(...)` — see the `scan_range` doc-comment for why this method
    /// takes `&[u8]`.
    async fn read_as_of(
        &self,
        scope: &Scope,
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError>;

    /// Queue publish. Returns the offset assigned by the broker (monotonic within
    /// `(topic, partition)`).
    async fn publish(
        &self,
        scope: &Scope,
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
        scope: &Scope,
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
        scope: &Scope,
        _topic: &str,
        _partition: u16,
    ) -> Result<u64, StorageError> {
        let _ = scope;
        Err(StorageError::NotSupported("queue_depth not implemented for this StoragePort backend"))
    }

    /// Enumerate known scopes under an optional `prefix`, paginated.
    ///
    /// Returns a [`ScopePage`] whose `scopes` field is ordered ascending by
    /// scope string. When `next_cursor` is `Some`, the caller passes it back
    /// unchanged on the next call to advance pagination. `next_cursor == None`
    /// means the enumeration is exhausted.
    ///
    /// `limit` is a *hint*: backends may return fewer than `limit` scopes per
    /// page (e.g. Moon's `SCAN` returns batches whose size is driven by its
    /// own COUNT parameter; the implementation parses scopes out of those
    /// keys and dedupes per page). Backends MUST return at most `limit`
    /// scopes when more are available — callers cannot rely on a single
    /// call yielding everything.
    ///
    /// `cursor` is opaque to the caller (Q-U1 lock — backend-native cursor
    /// wrapped in base64). It MUST be the exact bytes returned by the
    /// previous call's `next_cursor`. Passing a corrupted cursor returns
    /// `Err(StorageError::Backend(_))`.
    ///
    /// ## Degradation contract
    ///
    /// Backends without scope enumeration return
    /// `Err(StorageError::NotSupported("list_scopes"))`. Higher layers
    /// (e.g. Helios viewer surfaces, `memories.search` cross-scope queries)
    /// degrade by accepting a caller-supplied scope list instead of
    /// auto-discovering. This is the documented escape hatch — callers
    /// MUST handle `NotSupported` and fall back to per-scope queries.
    ///
    /// **Postgres backend:** returns `NotSupported`. Per migration
    /// `20260510000005_scope_partitioning.sql`, every primitive table is
    /// `FORCE ROW LEVEL SECURITY`-protected with a policy
    /// `scope = current_setting('lunaris.scope', true)`. A cross-scope
    /// `SELECT DISTINCT scope` would either return zero rows (when the GUC
    /// is set) or require `SET row_security = off`, which Lunaris' app
    /// role does not hold. The contractual answer is to surface this
    /// degradation rather than weaken the RLS boundary.
    ///
    /// **Moon backend:** implements via `SCAN MATCH lunaris:*` + parsing
    /// the scope segment out of `lunaris:{scope}:{kind}:{ulid}` keys
    /// (Q-U2 lock — lazy SCAN-parse; no explicit scope index).
    ///
    /// **Embedded (SQLite) backend:** implements by scanning the `lunaris_kv`
    /// key column and parsing scopes out of the same `lunaris:{scope}:…`
    /// keyspace convention.
    async fn list_scopes(
        &self,
        prefix: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ScopePage, StorageError> {
        let _ = (prefix, limit, cursor);
        Err(StorageError::NotSupported(
            "list_scopes not implemented for this StoragePort backend",
        ))
    }

    /// Report capabilities so higher layers (retrievers, recipes, the conformance
    /// suite) can make degradation decisions per blueprint §6.
    fn capabilities(&self) -> StorageCapabilities;
}
