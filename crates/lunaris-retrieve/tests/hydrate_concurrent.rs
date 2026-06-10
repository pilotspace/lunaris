//! Plan 260610-f91: Concurrency guards for hydrate() and partial_hydrate_text().
//!
//! Uses a Barrier-based mock StoragePort: read_as_of waits until ALL concurrent
//! callers arrive at the barrier before returning. A serial implementation
//! deadlocks (only 1 caller at a time — barrier never fires). A concurrent
//! implementation fans out all calls before any awaits, releasing the barrier.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
};
use lunaris_retrieve::hydrate::{hydrate, partial_hydrate_text};
use lunaris_retrieve::types::RawHit;
use serde_json::json;
use tokio::sync::Barrier;
use ulid::Ulid;

// ─── BarrierStorage ──────────────────────────────────────────────────────────

/// A StoragePort that blocks each read_as_of call on a shared Barrier.
/// When concurrent_count callers all arrive simultaneously, the barrier releases.
/// A serial caller (one at a time) will NEVER release the barrier → deadlock.
struct BarrierStorage {
    barrier: Arc<Barrier>,
    chunks: HashMap<Vec<u8>, Vec<u8>>,
    /// When true, any read_as_of against an episode key (`:episode:` in the
    /// key) returns `Err(StorageError::Backend)`. Guards the error-propagation
    /// contract: a failing episode lookup must fail the whole hydrate call,
    /// not silently degrade to an empty `source`.
    fail_episode_reads: bool,
}

impl BarrierStorage {
    fn new(concurrent_count: usize, chunks: HashMap<Vec<u8>, Vec<u8>>) -> Self {
        Self {
            barrier: Arc::new(Barrier::new(concurrent_count)),
            chunks,
            fail_episode_reads: false,
        }
    }

    fn with_failing_episode_reads(mut self) -> Self {
        self.fail_episode_reads = true;
        self
    }
}

#[async_trait]
impl StoragePort for BarrierStorage {
    async fn read_as_of(
        &self,
        _scope: &Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        self.barrier.wait().await; // Blocks until `concurrent_count` callers arrive
        if self.fail_episode_reads && key.windows(9).any(|w| w == b":episode:") {
            return Err(StorageError::Backend("episode read failed (injected)".into()));
        }
        if let Some(v) = self.chunks.get(key).cloned() {
            return Ok(Some(Row {
                key: key.to_vec(),
                value: Bytes::from(v),
                bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            }));
        }
        Ok(None)
    }

    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _: &Scope,
        _: &str,
        _: &[f32],
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
        _: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Err(StorageError::NotSupported("BarrierStorage"))
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("BarrierStorage"))
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(vec![]).boxed())
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("BarrierStorage"))
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("BarrierStorage"))
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
        }
    }
}

#[async_trait]
impl KeywordPort for BarrierStorage {
    async fn keyword_search(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(vec![])
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_chunk_bytes(scope: &Scope) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use lunaris_core::primitives::Chunk;
    let clock = HlcClock::new(0);
    let ep_id = Ulid::new();
    let chunk = Chunk::new(scope.clone(), ep_id, "text", 4, 0, vec![], &clock);
    let id_bytes = chunk.id.to_bytes().to_vec();
    let key = lunaris_core::keyspace::chunk_key(scope, chunk.id);
    let val = serde_json::to_vec(&chunk).unwrap();
    (id_bytes, key, val)
}

fn raw_hit(id: Vec<u8>) -> RawHit {
    RawHit {
        id,
        score: 1.0,
        rerank_applied: false,
        degraded: false,
        metadata: json!({}),
        source_op: lunaris_retrieve::SourceOp::Vector,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// hydrate() must issue all chunk reads concurrently.
/// The Barrier is sized to the number of hits — serial code deadlocks.
#[tokio::test]
async fn hydrate_concurrent_chunk_reads() {
    let scope = Scope::dev();
    const N: usize = 3;
    let mut chunks = HashMap::new();
    let mut hits = Vec::new();
    for _ in 0..N {
        let (id_bytes, key, val) = make_chunk_bytes(&scope);
        chunks.insert(key, val);
        hits.push(raw_hit(id_bytes));
    }
    // Barrier sized to N: requires N concurrent waiters to fire.
    let storage = Arc::new(BarrierStorage::new(N, chunks));

    // Serial implementation will deadlock here; concurrent fan-out will not.
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        hydrate(storage.as_ref(), &scope, hits, None, false),
    )
    .await;

    assert!(result.is_ok(), "hydrate() timed out — reads are NOT concurrent (barrier deadlocked)");
    let hits_out = result.unwrap().unwrap();
    assert_eq!(hits_out.len(), N, "all N hits must be returned");
}

/// partial_hydrate_text() must issue all chunk reads concurrently.
#[tokio::test]
async fn partial_hydrate_text_concurrent_reads() {
    let scope = Scope::dev();
    const N: usize = 4;
    let mut chunks = HashMap::new();
    let mut hits = Vec::new();
    for _ in 0..N {
        let (id_bytes, key, val) = make_chunk_bytes(&scope);
        chunks.insert(key, val);
        hits.push(raw_hit(id_bytes));
    }
    let storage = Arc::new(BarrierStorage::new(N, chunks));

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        partial_hydrate_text(storage.as_ref(), &scope, &hits, None),
    )
    .await;

    assert!(result.is_ok(), "partial_hydrate_text() timed out — NOT concurrent");
    assert_eq!(result.unwrap().unwrap().len(), N);
}

/// hydrate() must preserve ranked hit order in output.
/// Seed 3 hits with distinct scores; verify output order matches input order.
#[tokio::test]
async fn hydrate_preserves_hit_order() {
    use lunaris_core::primitives::Chunk;
    let scope = Scope::dev();
    // Use 1-waiter barrier so it fires immediately — this test is about order, not concurrency.
    let mut chunks = HashMap::new();
    let clock = HlcClock::new(0);
    let mut id_order = Vec::new();
    for i in 0..3usize {
        let ep_id = Ulid::new();
        let chunk =
            Chunk::new(scope.clone(), ep_id, format!("text {i}").as_str(), 4, 0, vec![], &clock);
        let id_bytes = chunk.id.to_bytes().to_vec();
        let key = lunaris_core::keyspace::chunk_key(&scope, chunk.id);
        chunks.insert(key, serde_json::to_vec(&chunk).unwrap());
        id_order.push(id_bytes);
    }
    let hits: Vec<RawHit> = id_order
        .iter()
        .enumerate()
        .map(|(i, id)| RawHit {
            id: id.clone(),
            score: (3 - i) as f32, // 3, 2, 1 — descending, ranked
            rerank_applied: false,
            degraded: false,
            metadata: json!({}),
            source_op: lunaris_retrieve::SourceOp::Vector,
        })
        .collect();
    let storage = Arc::new(BarrierStorage::new(1, chunks));

    let out = hydrate(storage.as_ref(), &scope, hits, None, false).await.unwrap();
    assert_eq!(out.len(), 3);
    // Output ids must match input order (hits are ranked — order must be preserved).
    for (i, hit) in out.iter().enumerate() {
        assert_eq!(hit.id, id_order[i], "hit order must be preserved at index {i}");
    }
}

/// A storage error during the episode pass must FAIL the hydrate call —
/// pre-fan-out behavior (`.await?` in the serial loop) propagated episode
/// read errors, and the concurrent version must preserve that contract
/// rather than silently degrading `source` to "".
#[tokio::test]
async fn hydrate_propagates_episode_read_errors() {
    let scope = Scope::dev();
    let mut chunks = HashMap::new();
    let (id_bytes, key, val) = make_chunk_bytes(&scope);
    chunks.insert(key, val);
    let storage = Arc::new(BarrierStorage::new(1, chunks).with_failing_episode_reads());

    let result = hydrate(storage.as_ref(), &scope, vec![raw_hit(id_bytes)], None, false).await;
    assert!(
        result.is_err(),
        "episode read errors must propagate out of hydrate(), not be swallowed"
    );
}
