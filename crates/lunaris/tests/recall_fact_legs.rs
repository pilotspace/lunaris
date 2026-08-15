//! KG-RAG wiring Wave B (2026-07-21): the DEFAULT recall root gains fact
//! legs when (and only when) the graph pipeline is enabled.
//!
//! Wave A made `execute()` fact-aware (hydrate_mixed), but the default root
//! `Vector::new("chunks", 30)` never queries the `facts` index, so graph-ON
//! deployments still can't see their facts through `Lunaris::recall()` /
//! `recall_with_degraded_check()` (→ HTTP + MCP). The hook proved the fused
//! composition (`chunks ∧ facts → RRF`, hook context.rs::hybrid_root); this
//! wave promotes it to the core default — gated on
//! `graph_pipeline().is_enabled()` so the graph feature stays opt-in and the
//! graph-OFF hot path is byte-identical to today.
//!
//! RED until `Lunaris::recall()` composes the hybrid root under the toggle.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Lunaris, Query};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_extract::types::{EntityId, Fact};
use parking_lot::Mutex;
use ulid::Ulid;

/// Per-index canned vector hits + KV rows; records every index that
/// `vector_search` is asked to query so the opt-in gate is provable.
#[derive(Default)]
struct IndexedStorage {
    hits_by_index: Mutex<HashMap<String, Vec<VectorHit>>>,
    rows_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    queried_indexes: Mutex<Vec<String>>,
}

impl IndexedStorage {
    fn seed_chunk(&self, scope: &Scope, id: Ulid, ep_id: Ulid, text: &str, clock: &HlcClock) {
        let chunk = lunaris_core::Chunk::new(scope.clone(), ep_id, text, 4, 0, vec![], clock);
        let mut chunk_val = serde_json::to_value(&chunk).unwrap();
        chunk_val["id"] = serde_json::Value::String(id.to_string());
        self.rows_by_key.lock().insert(
            lunaris_core::keyspace::chunk_key(scope, id),
            serde_json::to_vec(&chunk_val).unwrap(),
        );

        let ep = lunaris_core::Episode::new(scope.clone(), "test:source", "episode text", clock);
        let mut ep_val = serde_json::to_value(&ep).unwrap();
        ep_val["id"] = serde_json::Value::String(ep_id.to_string());
        self.rows_by_key.lock().insert(
            lunaris_core::keyspace::episode_key(scope, ep_id),
            serde_json::to_vec(&ep_val).unwrap(),
        );
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
        self.rows_by_key.lock().insert(
            lunaris_core::keyspace::fact_key(scope, id),
            serde_json::to_vec(&fact).unwrap(),
        );
    }
}

#[async_trait]
impl StoragePort for IndexedStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        self.queried_indexes.lock().push(index.to_owned());
        Ok(self.hits_by_index.lock().get(index).cloned().unwrap_or_default())
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
        Err(StorageError::NotSupported("IndexedStorage::publish"))
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("IndexedStorage::subscribe"))
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
impl KeywordPort for IndexedStorage {
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

struct Fixture {
    storage: Arc<IndexedStorage>,
    handle: Lunaris,
    scope: Scope,
}

fn fixture() -> Fixture {
    let scope = Scope::new("fact-legs-t").unwrap();
    let clock = HlcClock::new(0);
    let chunk_id = Ulid::new();
    let fact_id = Ulid::new();
    let ep_id = Ulid::new();

    let storage = Arc::new(IndexedStorage::default());
    storage.seed_chunk(&scope, chunk_id, ep_id, "the chunk text", &clock);
    storage.seed_fact(&scope, fact_id, "prefers", "Tin prefers admin-rebase merges");
    {
        let mut hits = storage.hits_by_index.lock();
        hits.insert("chunks".into(), vec![vh(chunk_id, 0.90)]);
        hits.insert("facts".into(), vec![vh(fact_id, 0.80)]);
    }

    let handle = Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage.clone() as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        clock,
    );
    Fixture { storage, handle, scope }
}

/// Graph pipeline ON → the default recall root fuses `chunks ∧ facts`, so
/// the fact hit reaches the caller through the production path
/// (`ScopedLunaris::recall` → `Lunaris::recall()` → execute → hydrate_mixed).
#[tokio::test]
async fn graph_on_default_recall_returns_fact_hits() {
    let f = fixture();
    f.handle.graph_pipeline().enable();

    let hits = f
        .handle
        .scoped(f.scope.clone())
        .recall(Query::text("what does Tin prefer?"))
        .await
        .unwrap();

    let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
    assert!(texts.contains(&"the chunk text"), "chunk leg must still contribute; got {texts:?}");
    assert!(
        texts.contains(&"Tin prefers admin-rebase merges"),
        "graph-ON default recall must surface the fact hit via the fused facts leg; got {texts:?}"
    );
    let fact_hit = hits.iter().find(|h| h.source == "fact:prefers");
    assert!(fact_hit.is_some(), "fact hit must carry source=fact:{{predicate}}; got {hits:?}");
}

/// Graph pipeline OFF (default) → root is unchanged: chunks only, and the
/// `facts` index is NEVER queried. Pins the opt-in gate so the graph-OFF
/// hot path pays zero extra search cost.
#[tokio::test]
async fn graph_off_default_recall_never_touches_facts_index() {
    let f = fixture();
    assert!(!f.handle.graph_pipeline().is_enabled(), "pipeline must default OFF");

    let hits = f.handle.scoped(f.scope.clone()).recall(Query::text("anything")).await.unwrap();

    assert_eq!(hits.len(), 1, "graph-OFF recall returns only the chunk hit; got {hits:?}");
    assert_eq!(hits[0].text, "the chunk text");
    let queried = f.storage.queried_indexes.lock().clone();
    assert!(
        !queried.iter().any(|i| i == "facts"),
        "graph-OFF recall must not query the facts index (opt-in contract); queried: {queried:?}"
    );
}
