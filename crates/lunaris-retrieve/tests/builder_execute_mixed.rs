//! KG-RAG wiring Wave A (2026-07-21): `RetrievalBuilder::execute()` must be
//! fact-aware.
//!
//! Motivation (3-agent research synthesis, verified against source): the
//! graph-ON ingest pipeline writes Fact rows + a `facts` vector/BM25 index,
//! and the hook's fused hybrid root proves fact legs retrieve — but
//! `execute()` (builder.rs) hydrates through chunk-only `hydrate()`, so
//! every fact id is silently dropped before ANY caller of the core recall
//! path (`Lunaris::recall()`, HTTP `/v1/recall` incl. `mode:"graph"`, MCP
//! `memory_recall`) can see it. Only `lunaris-hook` bypasses this via
//! `hydrate_mixed`.
//!
//! Contract under test: `execute()` resolves hits through the SAME
//! heterogeneous read model as the hook (`hydrate_mixed`):
//! - chunk ids → byte-identical legacy chunk semantics (pinned separately by
//!   `hydrate_mixed.rs::chunk_only_matches_existing_hydrate`);
//! - fact ids → `text = fact_text`, `source = "fact:{predicate}"`;
//! - unknown ids → dropped, as before.
//!
//! RED until builder.rs swaps `hydrate` → `hydrate_mixed`.

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
use lunaris_extract::types::{EntityId, Fact};
use lunaris_retrieve::{Query, RetrievalBuilder, Vector};
use parking_lot::Mutex;
use ulid::Ulid;

/// HashMap-backed storage whose `vector_search` replays seeded hits and whose
/// `read_as_of` serves chunk, episode, AND fact rows (the at-rest
/// `lunaris_extract::Fact` shape, exactly what graph-ON ingest KvPuts).
#[derive(Default)]
struct MixedStorage {
    vector_hits: Mutex<Vec<VectorHit>>,
    rows_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl MixedStorage {
    fn seed_chunk(&self, scope: &Scope, id: Ulid, ep_id: Ulid, text: &str, clock: &HlcClock) {
        let chunk = lunaris_core::Chunk::new(scope.clone(), ep_id, text, 4, 0, vec![], clock);
        let mut chunk_val = serde_json::to_value(&chunk).unwrap();
        chunk_val["id"] = serde_json::Value::String(id.to_string());
        let key = lunaris_core::keyspace::chunk_key(scope, id);
        self.rows_by_key.lock().insert(key, serde_json::to_vec(&chunk_val).unwrap());

        let ep = lunaris_core::Episode::new(scope.clone(), "test:source", "episode text", clock);
        let mut ep_val = serde_json::to_value(&ep).unwrap();
        ep_val["id"] = serde_json::Value::String(ep_id.to_string());
        let ep_key = lunaris_core::keyspace::episode_key(scope, ep_id);
        self.rows_by_key.lock().insert(ep_key, serde_json::to_vec(&ep_val).unwrap());
    }

    fn seed_fact(&self, scope: &Scope, id: Ulid, predicate: &str, fact_text: &str) {
        let fact = Fact {
            id,
            subject_id: EntityId([1u8; 16]),
            predicate: predicate.to_owned(),
            object_id: EntityId([2u8; 16]),
            fact_text: fact_text.to_owned(),
            confidence: 0.9,
            valid_from_iso: "2026-07-14T00:00:00Z".to_owned(),
            valid_to_iso: None,
        };
        let key = lunaris_core::keyspace::fact_key(scope, id);
        self.rows_by_key.lock().insert(key, serde_json::to_vec(&fact).unwrap());
    }
}

#[async_trait]
impl StoragePort for MixedStorage {
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
        Ok(self.rows_by_key.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }
    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("MixedStorage::publish"))
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("MixedStorage::subscribe"))
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
impl KeywordPort for MixedStorage {
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

/// A root whose hit list mixes a chunk id and a fact id (exactly what a
/// fused `chunks ∧ facts` hybrid root emits) must hydrate BOTH through
/// `execute()` — the fact as `text = fact_text`, `source = "fact:{predicate}"`.
#[tokio::test]
async fn execute_hydrates_fact_hits_not_just_chunks() {
    let scope = Scope::new("exec-mixed-t1").unwrap();
    let clock = HlcClock::new(0);
    let chunk_id = Ulid::new();
    let fact_id = Ulid::new();
    let ep_id = Ulid::new();

    let storage = Arc::new(MixedStorage::default());
    *storage.vector_hits.lock() = vec![vh(chunk_id, 0.90), vh(fact_id, 0.80)];
    storage.seed_chunk(&scope, chunk_id, ep_id, "the chunk text", &clock);
    storage.seed_fact(&scope, fact_id, "listens_on", "zephyr-relay listens on port 7443");

    let builder = RetrievalBuilder::new(
        storage.clone() as Arc<dyn StoragePort>,
        storage.clone() as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
    )
    .with_scope(scope)
    .with_root(Vector::new("chunks", 30));

    let hits = builder.execute(Query::text("q")).await.unwrap();

    assert_eq!(
        hits.len(),
        2,
        "execute() must hydrate the fact hit alongside the chunk hit; \
         chunk-only hydrate() silently drops it. got: {hits:?}"
    );
    assert_eq!(hits[0].text, "the chunk text", "chunk semantics unchanged");
    assert_eq!(hits[0].source, "test:source", "chunk source = parent episode source");
    assert_eq!(hits[1].text, "zephyr-relay listens on port 7443", "fact text = fact_text");
    assert_eq!(hits[1].source, "fact:listens_on", "fact source = fact:{{predicate}}");
    assert!((hits[1].score - 0.80).abs() < 1e-6, "fused score preserved on the fact hit");
}

/// Ids that resolve to NEITHER a chunk nor a fact row must still be dropped
/// (the since-deleted-row skip) — fact-awareness must not resurrect garbage.
#[tokio::test]
async fn execute_still_drops_unknown_ids() {
    let scope = Scope::new("exec-mixed-t2").unwrap();
    let clock = HlcClock::new(0);
    let chunk_id = Ulid::new();
    let ep_id = Ulid::new();

    let storage = Arc::new(MixedStorage::default());
    *storage.vector_hits.lock() = vec![vh(chunk_id, 0.90), vh(Ulid::new(), 0.85)];
    storage.seed_chunk(&scope, chunk_id, ep_id, "only real chunk", &clock);

    let builder = RetrievalBuilder::new(
        storage.clone() as Arc<dyn StoragePort>,
        storage.clone() as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
    )
    .with_scope(scope)
    .with_root(Vector::new("chunks", 30));

    let hits = builder.execute(Query::text("q")).await.unwrap();
    assert_eq!(hits.len(), 1, "unknown id must stay dropped");
    assert_eq!(hits[0].text, "only real chunk");
}
