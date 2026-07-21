//! W5 task 2 — AS_OF pinning per retrieve execution.
//!
//! `RetrievalBuilder::execute()` / `execute_raw()` must pin ONE snapshot
//! timestamp when the caller supplies no `as_of` at all (neither
//! `Query::as_of` nor the builder's `.as_of(ts)`), and thread that SAME
//! instant through the entire fan-out: the operator tree (`ctx.query.as_of`)
//! AND the final `hydrate()` chunk/episode read pass.
//!
//! Before the W5 fix, `hydrate()` (and, independently, `Tree`'s BFS descent
//! and `rerank`'s `partial_hydrate_text`) each defaulted a missing `as_of` via
//! their OWN `HlcClock::new(0).tick()` call. A single fused retrieval tree
//! with multiple branches could straddle a concurrent invalidation: one
//! branch reads pre-supersede facts, another reads post-supersede facts —
//! all within the SAME logical `execute()` call. The fix pins one instant at
//! the seam where `query.as_of` is still `None` right before `QueryContext`
//! is constructed, so every downstream `ctx.query.as_of.unwrap_or_else(||
//! live_clock.tick())` fallback never fires — they all observe `Some(pinned)`.
//!
//! This test proves the invariant WITHOUT needing real clock skew: it
//! captures (a) the `as_of` the root operator observes via `ctx.query.as_of`
//! and (b) the `as_of` `hydrate()`'s `read_as_of` call receives, and asserts
//! they are the exact same `Some(Hlc)` — pre-fix they diverge (root sees
//! `None`, hydrate sees its own independently-ticked `Some(_)`).

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, LunarisError, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_retrieve::{Query, QueryContext, RawHit, RetrievalBuilder, Retriever, SourceOp};
use parking_lot::Mutex;
use serde_json::json;

/// Minimal storage double: records the `as_of` `Hlc` passed to every
/// `read_as_of` call (that's the snapshot `hydrate()` uses for its chunk /
/// episode reads) and always returns `Ok(None)` (missing row) — `hydrate()`'s
/// documented behavior for a missing chunk is to SKIP the hit, so the
/// pinning assertion doesn't need any seeded chunk data, only the requested
/// timestamp.
#[derive(Default)]
struct AsOfRecordingStorage {
    read_as_of_calls: Mutex<Vec<Hlc>>,
}

#[async_trait]
impl StoragePort for AsOfRecordingStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("AsOfRecordingStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        self.read_as_of_calls.lock().push(as_of);
        Ok(None)
    }
    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("AsOfRecordingStorage::publish"))
    }
    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("AsOfRecordingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for AsOfRecordingStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

/// Root operator stub that records the `ctx.query.as_of` it observed and
/// returns one `RawHit` so `hydrate()` actually issues a `read_as_of` call.
struct RecordingRoot {
    seen_as_of: Arc<Mutex<Option<Option<Hlc>>>>,
}

#[async_trait]
impl Retriever for RecordingRoot {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        *self.seen_as_of.lock() = Some(ctx.query.as_of);
        Ok(vec![RawHit {
            id: ulid::Ulid::new().to_bytes().to_vec(),
            score: 1.0,
            rerank_applied: false,
            degraded: false,
            metadata: json!({}),
            source_op: SourceOp::Vector,
        }])
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn build(
    storage: Arc<AsOfRecordingStorage>,
    seen_as_of: Arc<Mutex<Option<Option<Hlc>>>>,
) -> RetrievalBuilder {
    let root = RecordingRoot { seen_as_of };
    let storage_dyn: Arc<dyn StoragePort> = storage.clone();
    let keyword_dyn: Arc<dyn KeywordPort> = storage;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    RetrievalBuilder::new(storage_dyn, keyword_dyn, embedder).with_root(root)
}

#[tokio::test]
async fn as_of_none_is_pinned_once_and_matches_hydrate_snapshot() {
    let storage = Arc::new(AsOfRecordingStorage::default());
    let seen_as_of = Arc::new(Mutex::new(None));
    let builder = build(storage.clone(), seen_as_of.clone());

    // `Query::text` defaults `as_of = None`; the builder has no
    // `.as_of(ts)` override either — this is the "caller supplied nothing"
    // case the pinning fix targets.
    let hits = builder.execute(Query::text("q")).await.unwrap();
    assert_eq!(hits.len(), 0, "AsOfRecordingStorage::read_as_of always returns None (missing row)");

    let observed_by_root = seen_as_of.lock().expect("root retriever must have run");
    let pinned = observed_by_root.expect(
        "AS_OF pinning (W5 task 2): ctx.query.as_of must be Some(_) by the time root.retrieve() \
         runs, even when the caller supplied no as_of at all — RetrievalBuilder::execute() must \
         pin ONE snapshot at the seam before constructing QueryContext",
    );

    // KG-RAG Wave A: hydration is fact-aware (`hydrate_mixed`) — a missing id
    // probes chunk_key THEN fact_key, so the single unresolvable hit issues
    // exactly two read_as_of calls. The invariant under test is unchanged:
    // EVERY hydration read must use the one pinned snapshot.
    let hydrate_calls = storage.read_as_of_calls.lock().clone();
    assert_eq!(
        hydrate_calls.len(),
        2,
        "fact-aware hydration probes chunk_key then fact_key for the single unresolvable hit"
    );
    for call in &hydrate_calls {
        assert_eq!(
            *call, pinned,
            "every hydration read_as_of snapshot must be the EXACT SAME pinned instant the root \
             operator observed via ctx.query.as_of — pre-fix these come from INDEPENDENT \
             HlcClock::new(0) ticks and can diverge across a concurrent invalidation"
        );
    }
}

#[tokio::test]
async fn as_of_explicit_caller_value_is_never_overridden() {
    // Edge case per the task brief: a caller-supplied as_of must NEVER be
    // replaced by the pinning logic.
    let storage = Arc::new(AsOfRecordingStorage::default());
    let seen_as_of = Arc::new(Mutex::new(None));
    let builder = build(storage.clone(), seen_as_of.clone());

    let explicit = Hlc::from_parts(123_456_789, 7, 0);
    let mut query = Query::text("q");
    query.as_of = Some(explicit);

    let _ = builder.execute(query).await.unwrap();

    let observed = seen_as_of.lock().expect("root must have run").expect("must be Some");
    assert_eq!(observed, explicit, "caller-supplied as_of must be threaded through unchanged");
    assert_eq!(
        storage.read_as_of_calls.lock()[0],
        explicit,
        "hydrate() must use the caller's explicit as_of, not a freshly pinned one"
    );
}

#[tokio::test]
async fn as_of_builder_default_is_never_overridden() {
    // Same edge case, but the as_of comes from `RetrievalBuilder::as_of(ts)`
    // (the builder-level default) rather than `Query::as_of` directly.
    let storage = Arc::new(AsOfRecordingStorage::default());
    let seen_as_of = Arc::new(Mutex::new(None));
    let builder = build(storage.clone(), seen_as_of.clone());

    let explicit = Hlc::from_parts(987_654_321, 3, 0);
    let builder = builder.as_of(explicit);

    let _ = builder.execute(Query::text("q")).await.unwrap();

    let observed = seen_as_of.lock().expect("root must have run").expect("must be Some");
    assert_eq!(observed, explicit, "builder .as_of(ts) must be threaded through unchanged");
}

#[tokio::test]
async fn execute_raw_also_pins_as_of_once() {
    // `execute_raw` skips `hydrate()` but the operator tree still needs a
    // single pinned instant threaded through `ctx.query.as_of`.
    let storage = Arc::new(AsOfRecordingStorage::default());
    let seen_as_of = Arc::new(Mutex::new(None));
    let builder = build(storage.clone(), seen_as_of.clone());

    let raw = builder.execute_raw(Query::text("q")).await.unwrap();
    assert_eq!(raw.len(), 1);

    let observed = seen_as_of.lock().expect("root must have run");
    assert!(
        observed.is_some(),
        "execute_raw must also pin as_of before the root operator runs, not leave it None"
    );
}
