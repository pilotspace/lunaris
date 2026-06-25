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
    CypherQuery, Filter, GraphDecay, GraphResult, HotKey, Lsn, NavigateHit, NavigateSpec, QueueMsg,
    Row, ScopePage, VectorHit, WriteOp,
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

    /// Graph traversal with optional recency decay (ADD task
    /// `graph-decay-recency`, contract v1).
    ///
    /// **Additive trait method** (mirrors the `queue_depth` precedent):
    /// - `decay == None` delegates byte-for-byte to [`Self::graph_traverse`],
    ///   so every existing backend/mock keeps its exact behavior unchanged.
    /// - `decay == Some(_)` on the default impl returns
    ///   `Err(StorageError::NotSupported("graph_decay_unsupported…"))` —
    ///   callers gate on `capabilities().graph_decay_native` first.
    ///
    /// The Moon backend overrides this to append `--decay <λ>`
    /// (and `--time-weight <w>` when set) to `GRAPH.QUERY`, composing with
    /// `--params` and `VALID_AT` in a single command. Effective edge cost on
    /// Moon's read path becomes `|weight| + λ·w·age_seconds`; Moon rejects the
    /// flag on write Cypher server-side (surfaced as `StorageError::Backend`).
    async fn graph_traverse_decayed(
        &self,
        scope: &Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
        decay: Option<&GraphDecay>,
    ) -> Result<GraphResult, StorageError> {
        match decay {
            None => self.graph_traverse(scope, query, as_of).await,
            Some(_) => Err(StorageError::NotSupported(
                "graph_decay_unsupported: backend has no native decay traversal",
            )),
        }
    }

    /// Graph-expanded vector retrieval (ADD task `ft-navigate-recall`,
    /// contract v1): KNN seeds → server-side BFS over the scope graph →
    /// re-ranked hits with hop metadata.
    ///
    /// **Additive trait method** (mirrors `queue_depth` / `graph_traverse_decayed`):
    /// the default returns `Err(StorageError::NotSupported("graph_navigate_unsupported…"))`
    /// so every existing impl compiles unchanged — callers gate on
    /// `capabilities().graph_navigate_native` (the DSL `Navigate` operator
    /// degrades to plain `vector_search`).
    ///
    /// The Moon backend overrides this with raw
    /// `FT.NAVIGATE <index> "*=>[KNN k @vec $v]" PARAMS 2 v <blob> HOPS n
    /// [HOP_PENALTY p] [DECAY λ]`. Graph expansion only reaches nodes whose
    /// `_key` property links them to an FT doc (written by the `GraphNode`
    /// ADDNODE path) — indexes without graph-linked docs return KNN-only
    /// hits with `hop_depth == 0`, never an error.
    async fn vector_navigate(
        &self,
        scope: &Scope,
        index: &str,
        query: &[f32],
        k: usize,
        spec: &NavigateSpec,
    ) -> Result<Vec<NavigateHit>, StorageError> {
        let _ = (scope, index, query, k, spec);
        Err(StorageError::NotSupported(
            "graph_navigate_unsupported: backend has no native navigate",
        ))
    }

    /// Top sampled hot keys from the backend's frequency sketch (additive,
    /// queue_depth precedent — default `NotSupported`, no capability flag;
    /// the lunaris-server poller warns once and goes quiet).
    ///
    /// SCOPE-LESS BY DESIGN: this is an operator/server-global observability
    /// view (which keys dominate backend traffic across ALL scopes), not
    /// tenant data. It must never be exposed on a tenant-facing surface —
    /// lunaris-server aggregates the raw keys into bounded
    /// `(scope, kind)` Prometheus labels and drops everything unparseable.
    ///
    /// The Moon backend overrides this with raw `HOTKEYS COUNT <n>`
    /// (`n` clamped to Moon's 1..=128 sketch capacity): 1-in-64 sampled,
    /// SpaceSaving top-K, cumulative since Moon process start. An empty
    /// reply (cold server / `MOON_NO_HOTKEYS=1` kill switch) is `Ok(vec![])`,
    /// never an error.
    async fn hot_keys(&self, count: usize) -> Result<Vec<HotKey>, StorageError> {
        let _ = count;
        Err(StorageError::NotSupported("hot_keys_unsupported: backend has no hot-key sketch"))
    }

    /// Cheap liveness probe for the `/healthz` rollout-cutback surface
    /// (`observability-rollout-maturity`). SCOPE-LESS BY DESIGN — operator-global
    /// like [`Self::hot_keys`]; `/healthz` is unauthenticated so there is no
    /// tenant scope to thread.
    ///
    /// **Additive default = `Ok(())`** (mirrors the `queue_depth` / `hot_keys`
    /// precedent): an in-process or otherwise un-probeable backend (SQLite,
    /// in-memory, test mocks) reports healthy and keeps compiling unchanged. The
    /// Moon backend OVERRIDES this with a real `PING` round-trip bounded by
    /// `LUNARIS_MOON_OP_TIMEOUT`, so a dead/stalled Moon surfaces as `Err` and
    /// `lunaris-server`'s `/healthz` answers 503 (Postgres `SELECT 1` is a
    /// PG-parity follow-up — live-PG is deferred).
    async fn health_check(&self) -> Result<(), StorageError> {
        Ok(())
    }

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
        Err(StorageError::NotSupported("list_scopes not implemented for this StoragePort backend"))
    }

    /// Bulk-invalidate FT index records for a given `node_id` within an HLC wall-clock
    /// window `[hlc_wall_lo_inclusive, hlc_wall_hi_inclusive]`.
    ///
    /// Called by `Lunaris::invalidate_range` when Helios's `helios-git` detects a
    /// force-push or rebase and needs to evict stale recall from the agent's memory.
    ///
    /// ## Wire shape (Moon backend)
    ///
    /// ```text
    /// FT.INVALIDATE_RANGE <index> <node_id_field> <node_id_value>
    ///                     <hlc_wall_field> <hlc_wall_lo> <hlc_wall_hi>
    /// ```
    ///
    /// Returns the count of deleted records from that index. The Moon command returns
    /// `WRONGTYPE` when the index does not exist — callers MUST treat `WRONGTYPE` as
    /// a warn-and-skip (degraded mode), not a hard error.
    ///
    /// ## Closed-interval semantics
    ///
    /// Both `hlc_wall_lo_inclusive` and `hlc_wall_hi_inclusive` are **inclusive** bounds
    /// (matching Moon's `[lo, hi]` wire contract). Callers using half-open ranges (`lo..hi`)
    /// must subtract 1 from the exclusive upper bound before calling.
    ///
    /// ## Schema preconditions
    ///
    /// The target FT index must declare:
    /// - `hlc_node_id` as a `TAG` field, and
    /// - `hlc_wall` as a `NUMERIC` field.
    ///
    /// Without these schema fields, Moon's bitmap intersect returns empty and 0 is returned —
    /// a silent no-op, not an error. This is documented and expected for indices that predate
    /// the `helios-git` schema additions.
    ///
    /// ## Default (NotSupported)
    ///
    /// Backends other than Moon return `Err(StorageError::NotSupported(...))` via this
    /// default. `Lunaris::invalidate_range` treats `NotSupported` the same as a missing
    /// index — WARN and continue (degraded mode).
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
        let _ = (
            scope,
            index,
            node_id_field,
            node_id_value,
            hlc_wall_field,
            hlc_wall_lo_inclusive,
            hlc_wall_hi_inclusive,
        );
        Err(StorageError::NotSupported(
            "invalidate_range not implemented for this StoragePort backend",
        ))
    }

    /// Report capabilities so higher layers (retrievers, recipes, the conformance
    /// suite) can make degradation decisions per blueprint §6.
    fn capabilities(&self) -> StorageCapabilities;

    // ── HOOK-05: idempotency helpers (default = no-op for Moon/Postgres) ──

    /// Idempotency read: look up a previously-committed dedupe key.
    ///
    /// Default implementation returns `Ok(None)` (no dedupe — always fresh ingest).
    /// `EmbeddedStorage` overrides with a real SQLite lookup.
    /// Moon and Postgres use the default no-op (v0.5 scope: SQLite-only idempotency).
    ///
    /// The lookup is READ-ONLY. INGEST-04 is preserved — this method never calls
    /// `atomic_write`. (W6 fix: trait method replaces any `as_any()` downcast approach.)
    async fn lookup_by_dedupe_key(
        &self,
        scope: &Scope,
        dedupe_key: &str,
    ) -> Result<Option<Lsn>, StorageError> {
        let _ = (scope, dedupe_key);
        Ok(None)
    }

    /// Idempotency write: record a dedupe key after a successful `atomic_write`.
    ///
    /// Default implementation is a no-op.
    /// `EmbeddedStorage` overrides with a real SQLite `INSERT OR IGNORE`.
    ///
    /// This is a BEST-EFFORT post-`atomic_write` write (T-24-03-06): if the process
    /// is killed between the `atomic_write` commit and this call, replay will produce
    /// a duplicate Episode. Mitigation deferred to v0.6.
    async fn insert_dedupe_key(
        &self,
        scope: &Scope,
        dedupe_key: &str,
        lsn: Lsn,
    ) -> Result<(), StorageError> {
        let _ = (scope, dedupe_key, lsn);
        Ok(())
    }

    /// Batch KV get for embedding-cache lookups.
    ///
    /// Returns one `Option<Vec<u8>>` per entry in `keys`, in the same order.
    /// `Some(bytes)` → cache hit; `None` → cache miss.
    ///
    /// **Additive default** (mirrors `lookup_by_dedupe_key`): returns `Ok(vec![None; keys.len()])`
    /// so every existing backend compiles unchanged — the ingest dedup path degrades to a
    /// full embed pass on backends that do not override this method.
    ///
    /// Callers MUST treat `Err(_)` identically to all-None: log a warning and fall back to
    /// embedding. Cache read errors must NEVER fail an ingest.
    async fn kv_get_many(
        &self,
        scope: &Scope,
        keys: &[Vec<u8>],
    ) -> Result<Vec<Option<Vec<u8>>>, StorageError> {
        let _ = scope;
        Ok(vec![None; keys.len()])
    }
}
