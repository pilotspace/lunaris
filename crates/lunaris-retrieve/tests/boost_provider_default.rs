//! ADD task activation-ledger — the ONE pre-pinned default-behavior
//! assertion from §4 test_plan's `boost_provider.rs` row (scenario 6's "And"
//! clause: "a builder with only `with_boost_cache` behaves byte-identically
//! to pre-task behavior").
//!
//! Deliberately isolated in its OWN file so it compiles and passes
//! independently of whether `BoostProvider` / `with_boost_provider` exist —
//! it exercises ONLY the pre-existing Phase 14.2 `with_boost_cache` seam.
//! This is the "must be GREEN before build" evidence: proof the pre-existing
//! LRU boost pass is untouched by the activation-ledger work, captured
//! BEFORE any new-API code lands.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_retrieve::{Query, RetrievalBuilder, Vector};
use parking_lot::Mutex;
use ulid::Ulid;

#[derive(Default)]
struct RecordingStorage {
    vector_hits: Mutex<Vec<VectorHit>>,
    chunks_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    episodes_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl RecordingStorage {
    fn seed_chunk(&self, scope: &Scope, id: Ulid, ep_id: Ulid, text: &str, clock: &HlcClock) {
        let chunk = lunaris_core::Chunk::new(scope.clone(), ep_id, text, 4, 0, vec![], clock);
        let mut chunk_val = serde_json::to_value(&chunk).unwrap();
        chunk_val["id"] = serde_json::Value::String(id.to_string());
        let key = lunaris_core::keyspace::chunk_key(scope, id);
        self.chunks_by_key.lock().insert(key, serde_json::to_vec(&chunk_val).unwrap());

        let ep = lunaris_core::Episode::new(scope.clone(), "test:source", "episode text", clock);
        let mut ep_val = serde_json::to_value(&ep).unwrap();
        ep_val["id"] = serde_json::Value::String(ep_id.to_string());
        let ep_key = lunaris_core::keyspace::episode_key(scope, ep_id);
        self.episodes_by_key.lock().insert(ep_key, serde_json::to_vec(&ep_val).unwrap());
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
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
        Ok(self.vector_hits.lock().clone())
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
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
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        if let Some(v) = self.chunks_by_key.lock().get(key).cloned() {
            return Ok(Some(Row {
                key: key.to_vec(),
                value: Bytes::from(v),
                bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            }));
        }
        if let Some(v) = self.episodes_by_key.lock().get(key).cloned() {
            return Ok(Some(Row {
                key: key.to_vec(),
                value: Bytes::from(v),
                bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            }));
        }
        Ok(None)
    }
    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::publish"))
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::subscribe"))
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
impl KeywordPort for RecordingStorage {
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

fn vh(id: Ulid, score: f32) -> VectorHit {
    VectorHit {
        id: id.to_bytes().to_vec(),
        score,
        rerank_applied: false,
        metadata: serde_json::json!({}),
    }
}

/// A builder that only wires `with_boost_cache` (the pre-existing Phase 14.2
/// seam, NEVER `with_boost_provider`) must behave EXACTLY as it did before
/// this task: no cache entry for the hit ⇒ score is untouched and ordering
/// is the raw vector-search order.
#[tokio::test]
async fn no_provider_wired_is_byte_identical() {
    let scope = Scope::new("test.boost-default-unchanged").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let ep = Ulid::new();

    let rec = Arc::new(RecordingStorage::default());
    *rec.vector_hits.lock() = vec![vh(id_a, 0.90), vh(id_b, 0.80)];
    rec.seed_chunk(&scope, id_a, ep, "chunk A", &clock);
    rec.seed_chunk(&scope, id_b, ep, "chunk B", &clock);

    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));

    // Empty boost cache: present (wired) but carries no entries for these ids.
    let boost_cache: Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>> = Arc::new(
        parking_lot::RwLock::new(lru::LruCache::new(std::num::NonZeroUsize::new(8).unwrap())),
    );

    let builder = RetrievalBuilder::new(storage, keyword, embedder)
        .with_scope(scope)
        .with_root(Vector::new("chunks", 30))
        .with_boost_cache(boost_cache);
    let hits = builder.execute(Query::text("q")).await.unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].text, "chunk A", "raw vector order preserved: A(0.90) before B(0.80)");
    assert!(
        (hits[0].score - 0.90).abs() < 1e-6,
        "A.score must be untouched at 0.90; got {}",
        hits[0].score
    );
    assert!(
        (hits[1].score - 0.80).abs() < 1e-6,
        "B.score must be untouched at 0.80; got {}",
        hits[1].score
    );
}
