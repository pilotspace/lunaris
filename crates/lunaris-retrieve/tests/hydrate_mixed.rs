//! ADD task `hook-recall-graph-hybrid` (contract FROZEN @ v1.1, 2026-07-14):
//! `hydrate_mixed` — fact-aware hydration for fused hybrid recall.
//!
//! The existing `hydrate` is CHUNK-ONLY (hydrate.rs — non-chunk ids are
//! silently dropped), so a fused root that includes the `facts` legs loses
//! every fact hit at hydration time. `hydrate_mixed` keeps chunk semantics
//! byte-identical and additionally resolves fact ids via
//! `lunaris_core::keyspace::fact_key` to the at-rest `lunaris_extract::Fact`
//! row (heterogeneous read model): text = fact_text, source =
//! "fact:{predicate}".
//!
//! COMPILE-RED until `hydrate_mixed` lands — confined to this test binary.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::keyspace::{chunk_key, episode_key, fact_key};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Episode, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
};
use lunaris_extract::types::{EntityId, Fact};
use lunaris_retrieve::hydrate::hydrate_mixed;
use lunaris_retrieve::types::{RawHit, SourceOp};
use ulid::Ulid;

// ─── KvStorage — HashMap-backed read_as_of mock ─────────────────────────────

struct KvStorage {
    rows: HashMap<Vec<u8>, Vec<u8>>,
}

#[async_trait]
impl StoragePort for KvStorage {
    async fn read_as_of(
        &self,
        _scope: &Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
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
        Err(StorageError::NotSupported("KvStorage"))
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("KvStorage"))
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
        Err(StorageError::NotSupported("KvStorage"))
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("KvStorage"))
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
impl KeywordPort for KvStorage {
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

// ─── Fixture ─────────────────────────────────────────────────────────────────

fn raw(id: Ulid, score: f32, op: SourceOp) -> RawHit {
    RawHit {
        id: id.to_bytes().to_vec(),
        score,
        rerank_applied: false,
        degraded: false,
        metadata: serde_json::json!({}),
        source_op: op,
    }
}

fn test_fact(id: Ulid, predicate: &str, fact_text: &str) -> Fact {
    Fact {
        id,
        subject_id: EntityId([1u8; 16]),
        predicate: predicate.to_owned(),
        object_id: EntityId([2u8; 16]),
        fact_text: fact_text.to_owned(),
        confidence: 0.9,
        valid_from_iso: "2026-07-14T00:00:00Z".to_owned(),
        valid_to_iso: None,
    }
}

/// Seed a chunk row + its parent episode row + a fact row; return
/// (storage, chunk_id, fact_id).
fn seeded(scope: &Scope) -> (Arc<KvStorage>, Ulid, Ulid) {
    use lunaris_core::primitives::Chunk;
    let clock = HlcClock::new(0);

    let episode = Episode::new(scope.clone(), "test:episode-source", "episode content", &clock);
    let chunk = Chunk::new(scope.clone(), episode.id, "the chunk text", 4, 0, vec![], &clock);
    let chunk_id = chunk.id;

    let fact_id = Ulid::new();
    let fact = test_fact(fact_id, "listens_on", "zephyr-relay listens on port 7443");

    let mut rows = HashMap::new();
    rows.insert(episode_key(scope, episode.id), serde_json::to_vec(&episode).unwrap());
    rows.insert(chunk_key(scope, chunk_id), serde_json::to_vec(&chunk).unwrap());
    rows.insert(fact_key(scope, fact_id), serde_json::to_vec(&fact).unwrap());
    (Arc::new(KvStorage { rows }), chunk_id, fact_id)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Rerank pair-encoding half: `partial_hydrate_text` (the cross-encoder's
/// candidate-text source, rerank.rs) must resolve FACT ids to their
/// `fact_text` — with chunk-only hydration a fact hit gets no entry, the
/// reranker pair-encodes it against an empty string, sigmoid ≈ 0, and every
/// fact sinks below `.top(k)` before the reader ever sees it.
#[tokio::test]
async fn partial_hydrate_text_resolves_fact_text() {
    let scope = Scope::new("hydrate-mixed-pht").unwrap();
    let (storage, chunk_id, fact_id) = seeded(&scope);

    let hits = vec![raw(chunk_id, 0.032, SourceOp::Vector), raw(fact_id, 0.016, SourceOp::Keyword)];
    let texts =
        lunaris_retrieve::hydrate::partial_hydrate_text(storage.as_ref(), &scope, &hits, None)
            .await
            .expect("partial_hydrate_text");

    assert_eq!(
        texts.get(&chunk_id.to_bytes().to_vec()).map(String::as_str),
        Some("the chunk text"),
        "chunk semantics unchanged"
    );
    assert_eq!(
        texts.get(&fact_id.to_bytes().to_vec()).map(String::as_str),
        Some("zephyr-relay listens on port 7443"),
        "fact ids must hydrate to fact_text so the cross-encoder can score them"
    );
}

/// §2 root-shape scenario, hydration half: a fused hit-list mixing chunk ids
/// and fact ids hydrates BOTH — the chunk with existing `hydrate` semantics
/// (text + episode source), the fact from its `fact_key` row (text =
/// fact_text, source = "fact:{predicate}", fused score preserved).
#[tokio::test]
async fn mixed_hits_hydrate_chunk_and_fact() {
    let scope = Scope::new("hydrate-mixed-t1").unwrap();
    let (storage, chunk_id, fact_id) = seeded(&scope);

    let hits = vec![raw(chunk_id, 0.032, SourceOp::Vector), raw(fact_id, 0.016, SourceOp::Keyword)];
    let hydrated =
        hydrate_mixed(storage.as_ref(), &scope, hits, None, false).await.expect("hydrate_mixed");

    assert_eq!(hydrated.len(), 2, "both the chunk and the fact must hydrate");

    let chunk_hit =
        hydrated.iter().find(|h| h.id == chunk_id.to_bytes().to_vec()).expect("chunk hit present");
    assert_eq!(chunk_hit.text, "the chunk text");
    assert_eq!(chunk_hit.source, "test:episode-source", "chunk keeps episode-source semantics");

    let fact_hit =
        hydrated.iter().find(|h| h.id == fact_id.to_bytes().to_vec()).expect("fact hit present");
    assert_eq!(fact_hit.text, "zephyr-relay listens on port 7443", "fact text = fact_text");
    assert_eq!(fact_hit.source, "fact:listens_on", "fact source = fact:{{predicate}}");
    assert!((fact_hit.score - 0.016).abs() < f32::EPSILON, "fused RRF score preserved");
}

/// Chunk-only inputs must behave byte-identically to the existing `hydrate`
/// (regression pin for the legacy semantics `hydrate_mixed` wraps).
#[tokio::test]
async fn chunk_only_matches_existing_hydrate() {
    let scope = Scope::new("hydrate-mixed-t2").unwrap();
    let (storage, chunk_id, _) = seeded(&scope);

    let hits = vec![raw(chunk_id, 0.5, SourceOp::Vector)];
    let via_mixed = hydrate_mixed(storage.as_ref(), &scope, hits.clone(), None, false)
        .await
        .expect("hydrate_mixed");
    let via_hydrate =
        lunaris_retrieve::hydrate::hydrate(storage.as_ref(), &scope, hits, None, false)
            .await
            .expect("hydrate");

    assert_eq!(via_mixed.len(), 1);
    assert_eq!(via_hydrate.len(), 1);
    assert_eq!(via_mixed[0].text, via_hydrate[0].text);
    assert_eq!(via_mixed[0].source, via_hydrate[0].source);
    assert_eq!(via_mixed[0].heading_path, via_hydrate[0].heading_path);
    assert_eq!(via_mixed[0].valid_from, via_hydrate[0].valid_from);
}

/// An id resolving to NEITHER a chunk row NOR a fact row is dropped, never an
/// error (mirrors `hydrate`'s since-deleted-chunk skip semantics).
#[tokio::test]
async fn unknown_id_is_dropped() {
    let scope = Scope::new("hydrate-mixed-t3").unwrap();
    let (storage, chunk_id, _) = seeded(&scope);

    let hits = vec![raw(chunk_id, 0.5, SourceOp::Vector), raw(Ulid::new(), 0.4, SourceOp::Vector)];
    let hydrated =
        hydrate_mixed(storage.as_ref(), &scope, hits, None, false).await.expect("hydrate_mixed");

    assert_eq!(hydrated.len(), 1, "unknown id silently dropped");
    assert_eq!(hydrated[0].id, chunk_id.to_bytes().to_vec());
}

/// The `degraded` flag ORs into fact hits exactly as it does for chunk hits
/// (Plan 04-04 B-9 parity — verifier-queue-lag backpressure must not be
/// hidden on the new fact path).
#[tokio::test]
async fn initial_degraded_flag_reaches_fact_hits() {
    let scope = Scope::new("hydrate-mixed-t4").unwrap();
    let (storage, _, fact_id) = seeded(&scope);

    let hits = vec![raw(fact_id, 0.016, SourceOp::Keyword)];
    let hydrated =
        hydrate_mixed(storage.as_ref(), &scope, hits, None, true).await.expect("hydrate_mixed");

    assert_eq!(hydrated.len(), 1);
    assert!(hydrated[0].degraded, "initial_degraded ORs into fact hits");
}
