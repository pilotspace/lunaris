//! ADD task activation-ledger — RED/GREEN suite for the `BoostProvider`
//! read seam (§3 CONTRACT, §4 test_plan).
//!
//! Mirrors `dsl_compose.rs`'s `RecordingStorage` fixture: a fake
//! `StoragePort` + `KeywordPort` returning canned vector hits and seeded
//! chunk/episode rows so `RetrievalBuilder::execute()` runs end-to-end
//! without a real backend.
//!
//! `no_provider_wired_is_byte_identical` (the pre-pinned default-behavior
//! assertion covering scenario 6's "And" clause) lives in the SEPARATE file
//! `boost_provider_default.rs` — it exercises only the pre-existing
//! `with_boost_cache` seam and must compile+pass independently of whether
//! `BoostProvider` exists yet, so it can be proven GREEN before this file's
//! new-symbol RED lands.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
use lunaris_retrieve::{BoostProvider, Query, RetrievalBuilder, Vector};
use parking_lot::Mutex;
use ulid::Ulid;

// ============================================================ Fixture

#[derive(Default)]
struct RecordingStorage {
    vector_hits: Mutex<Vec<VectorHit>>,
    chunks_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    episodes_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl RecordingStorage {
    fn seed_chunk(&self, scope: &Scope, id: Ulid, ep_id: Ulid, text: &str, clock: &HlcClock) {
        let chunk = lunaris_core::Chunk::new(scope.clone(), ep_id, text, 4, 0, vec![], clock);
        // Force the id so it matches the caller-chosen `id` (Chunk::new mints
        // its own fresh Ulid otherwise).
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

fn build_parts(
    rec: Arc<RecordingStorage>,
) -> (Arc<dyn StoragePort>, Arc<dyn KeywordPort>, Arc<dyn Embedder>) {
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    (storage, keyword, embedder)
}

/// Fixed-output fake `BoostProvider` that also counts `priors()` calls.
struct FixedProvider {
    out: HashMap<Ulid, f32>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl BoostProvider for FixedProvider {
    async fn priors(&self, _scope: &Scope, _ids: &[Ulid]) -> HashMap<Ulid, f32> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.out.clone()
    }
}

// ============================================================ Tests

/// Scenario 3 (unit level) — two equal-similarity hits; the provider gives
/// ONLY the first-seeded id a positive prior. That id must rank first after
/// the boost pass even though the raw vector scores tied.
#[tokio::test]
async fn provider_prior_flips_equal_similarity_order() {
    let scope = Scope::new("test.boost-provider-flip").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let ep = Ulid::new();

    let rec = Arc::new(RecordingStorage::default());
    // B is returned FIRST from vector_search (both tie at 0.80) so a flip is
    // only explained by the provider prior, never by pre-existing order.
    *rec.vector_hits.lock() = vec![vh(id_b, 0.80), vh(id_a, 0.80)];
    rec.seed_chunk(&scope, id_a, ep, "chunk A", &clock);
    rec.seed_chunk(&scope, id_b, ep, "chunk B", &clock);

    let (storage, keyword, embedder) = build_parts(rec.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut prior = HashMap::new();
    prior.insert(id_a, 0.10_f32);
    let provider = Arc::new(FixedProvider { out: prior, calls: calls.clone() });

    let builder = RetrievalBuilder::new(storage, keyword, embedder)
        .with_scope(scope)
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider);
    let hits = builder.execute(Query::text("q")).await.unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].text, "chunk A",
        "A's provider prior must flip the pre-boost tie: {hits:#?}"
    );
    assert!(
        (hits[0].score - 0.90).abs() < 1e-5,
        "A.score must be 0.80 + 0.10 prior; got {}",
        hits[0].score
    );
}

/// Scenario 6 — provider prior AND LRU delta compose on the SAME hit: the
/// final score is `base + provider_prior + lru_delta`.
#[tokio::test]
async fn provider_and_lru_compose_prior_then_delta() {
    let scope = Scope::new("test.boost-provider-compose").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let ep = Ulid::new();

    let rec = Arc::new(RecordingStorage::default());
    *rec.vector_hits.lock() = vec![vh(id_a, 0.50)];
    rec.seed_chunk(&scope, id_a, ep, "chunk A", &clock);

    let (storage, keyword, embedder) = build_parts(rec.clone());
    let mut prior = HashMap::new();
    prior.insert(id_a, 0.10_f32);
    let provider = Arc::new(FixedProvider { out: prior, calls: Arc::new(AtomicUsize::new(0)) });

    let boost_cache: Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>> = Arc::new(
        parking_lot::RwLock::new(lru::LruCache::new(std::num::NonZeroUsize::new(8).unwrap())),
    );
    boost_cache.write().put((scope.clone(), id_a), 0.25_f32);

    let builder = RetrievalBuilder::new(storage, keyword, embedder)
        .with_scope(scope)
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider)
        .with_boost_cache(boost_cache);
    let hits = builder.execute(Query::text("q")).await.unwrap();

    assert_eq!(hits.len(), 1);
    let expected = 0.50_f32 + 0.10_f32 + 0.25_f32;
    assert!(
        (hits[0].score - expected).abs() < 1e-5,
        "score must be base+provider_prior+lru_delta = {expected}; got {}",
        hits[0].score
    );
}

/// Scenario 5 — `priors()` is called exactly ONCE per `execute()`, whatever
/// the hit count, proving the read is a single batched call rather than a
/// per-hit round trip.
#[tokio::test]
async fn provider_read_is_one_batched_call() {
    let scope = Scope::new("test.boost-provider-batched").unwrap();
    let clock = HlcClock::new(0);
    let ids: Vec<Ulid> = (0..5).map(|_| Ulid::new()).collect();
    let ep = Ulid::new();

    let rec = Arc::new(RecordingStorage::default());
    *rec.vector_hits.lock() = ids.iter().map(|&id| vh(id, 0.5)).collect();
    for (i, &id) in ids.iter().enumerate() {
        rec.seed_chunk(&scope, id, ep, &format!("chunk {i}"), &clock);
    }

    let (storage, keyword, embedder) = build_parts(rec.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(FixedProvider { out: HashMap::new(), calls: calls.clone() });

    let builder = RetrievalBuilder::new(storage, keyword, embedder)
        .with_scope(scope)
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider);
    let hits = builder.execute(Query::text("q")).await.unwrap();

    assert_eq!(hits.len(), 5);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "priors() must be called exactly once per execute()"
    );
}
