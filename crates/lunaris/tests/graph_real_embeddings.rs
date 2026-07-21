//! KG-RAG wiring Wave C (2026-07-21): graph-ON ingest must embed entities
//! and facts with the REAL handle embedder, not the `det_vec` hash stub.
//!
//! Motivation (3-agent research synthesis, verified): `ingest_episode_graph_on`
//! writes `VectorUpsert{entities}` / `VectorUpsert{facts}` with a
//! DefaultHasher-seeded PRNG vector (`det_vec`, ingest.rs). Geometrically
//! random vectors make the ENTITIES/FACTS HNSW legs meaningless — the facts
//! VECTOR leg of `hybrid_root` (Wave B) and any future FT.NAVIGATE seeding
//! retrieve noise. The hook has carried a "real-fact-embeddings follow-on"
//! note since hook-recall-graph-hybrid v1.1 (context.rs).
//!
//! Contract: entity vectors embed `name`, fact vectors embed `fact_text`,
//! via ONE batched `Embedder::embed_batch` pass at the existing fan-out site
//! (no second model pass, INGEST-04 single atomic_write untouched). Under
//! `StubEmbedder` the swap is invisible by construction (StubEmbedder::embed
//! IS det_vec) — so this test uses a marker embedder whose output det_vec
//! can never produce.
//!
//! RED until ingest.rs swaps `det_vec` → `embedder.embed_batch`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Extractor, Lunaris};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort,
};
use lunaris_extract::types::EntityId;
use lunaris_extract::{
    ChunkInput, Entity as ExtractEntity, Fact as ExtractFact, RawExtraction, RawExtractionBatch,
};
use parking_lot::Mutex;
use ulid::Ulid;

const DIM: usize = 768;

/// Embedder whose vectors `det_vec` can never produce: `v[0] = 1000.0 +
/// text.len()`, `v[1..] = 0.0`. det_vec output lives in [-1, 1].
struct MarkerEmbedder;

fn marker_vec(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = 1000.0 + text.len() as f32;
    v
}

#[async_trait]
impl Embedder for MarkerEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|s| marker_vec(s)).collect())
    }
}

/// Storage that records every atomic_write batch.
#[derive(Default)]
struct OpRecordingStorage {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

impl OpRecordingStorage {
    /// All VectorUpsert ops across all batches for the given index.
    fn vector_upserts_for(&self, index: &str) -> Vec<(Vec<u8>, Vec<f32>)> {
        self.batches
            .lock()
            .iter()
            .flatten()
            .filter_map(|op| match op {
                WriteOp::VectorUpsert { index: i, id, embedding, .. } if i == index => {
                    Some((id.clone(), embedding.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl StoragePort for OpRecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            if let WriteOp::KvPut { key, value } = op {
                self.rows.lock().insert(key.clone(), value.clone());
            }
        }
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }
    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
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
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }
    async fn publish(
        &self,
        _s: &lunaris_core::Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }
    async fn subscribe(
        &self,
        _s: &lunaris_core::Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("OpRecordingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: DIM as u32,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for OpRecordingStorage {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

/// Extractor emitting one entity + one fact with known texts.
struct OneEntityOneFact;

const ENTITY_NAME: &str = "Zephyr Relay";
const FACT_TEXT: &str = "zephyr-relay listens on port 7443";

#[async_trait]
impl Extractor for OneEntityOneFact {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        let by_chunk: Vec<RawExtraction> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    RawExtraction {
                        source_chunk_id: c.chunk_id,
                        entities: vec![ExtractEntity {
                            id: EntityId::from_name_and_type(ENTITY_NAME, "Service"),
                            name: ENTITY_NAME.into(),
                            aliases: vec![],
                            entity_type: "Service".into(),
                            confidence: 0.95,
                            valid_from_iso: "2024-01-01T00:00:00Z".into(),
                            valid_to_iso: None,
                        }],
                        relations: vec![],
                        facts: vec![ExtractFact {
                            id: Ulid::new(),
                            subject_id: EntityId::from_name_and_type(ENTITY_NAME, "Service"),
                            predicate: "listens_on".into(),
                            object_id: EntityId::from_name_and_type("7443", "Port"),
                            fact_text: FACT_TEXT.into(),
                            confidence: 0.9,
                            valid_from_iso: "2024-01-01T00:00:00Z".into(),
                            valid_to_iso: None,
                        }],
                    }
                } else {
                    RawExtraction {
                        source_chunk_id: c.chunk_id,
                        entities: vec![],
                        relations: vec![],
                        facts: vec![],
                    }
                }
            })
            .collect();
        Ok(RawExtractionBatch { by_chunk })
    }

    fn applies(&self) -> bool {
        true
    }
}

/// Graph-ON ingest must store REAL embedder vectors for the entity (over
/// `name`) and the fact (over `fact_text`) — not `det_vec` stubs.
#[tokio::test]
async fn graph_on_ingest_embeds_entities_and_facts_with_real_embedder() {
    let rec = Arc::new(OpRecordingStorage::default());
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec.clone() as Arc<dyn KeywordPort>,
        Arc::new(MarkerEmbedder) as Arc<dyn Embedder>,
        clock.clone(),
    );
    handle.graph_pipeline().enable();
    handle.graph_pipeline().set_extractor(Arc::new(OneEntityOneFact));

    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "real-embed.md",
        "# Notes\nThe zephyr relay listens on port 7443.",
        &clock,
    );
    handle.ingest(ep).await.expect("graph-ON ingest must succeed");

    let entity_ups = rec.vector_upserts_for("entities");
    assert_eq!(entity_ups.len(), 1, "exactly one entity VectorUpsert expected");
    assert_eq!(
        entity_ups[0].1,
        marker_vec(ENTITY_NAME),
        "entity embedding must be the REAL embedder's output over the entity name, \
         not the det_vec hash stub"
    );

    let fact_ups = rec.vector_upserts_for("facts");
    assert_eq!(fact_ups.len(), 1, "exactly one fact VectorUpsert expected");
    assert_eq!(
        fact_ups[0].1,
        marker_vec(FACT_TEXT),
        "fact embedding must be the REAL embedder's output over fact_text, \
         not the det_vec hash stub"
    );

    assert_eq!(
        rec.batches.lock().len(),
        1,
        "INGEST-04: still exactly ONE atomic_write for the whole ingest"
    );
}
