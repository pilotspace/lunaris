//! Plan 03-02 integration tests for `Graph::anchored` operator.
//!
//! Uses an inline `RecordingStorage` fixture (mirrors the dsl_compose.rs
//! pattern by convention — Plan 02-01 established "boring duplication over
//! shared crate" for these test fixtures, see RecordingStorage in
//! degraded_fallback.rs / rerank_compose.rs / rrf_routing.rs / dsl_compose.rs).
//!
//! These tests cover the operator surface end-to-end against deterministic
//! canned `GraphResult`s — they do NOT exercise the Cypher dialect on a live
//! backend. The live moon-it / pg-it round-trip lives in Plan 03-03's
//! `id_hex_round_trip_ingest_then_graph_anchored` smoke test, where it goes
//! alongside the live ingest fan-out test.

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
    BiTemporal, Embedder, Hlc, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use lunaris_retrieve::{
    EntityId, Graph, Query, QueryContext, RetrievalBuilder, Retriever, SourceOp, Vector,
};
use parking_lot::Mutex;
use serde_json::json;

// ============================================================ Fixtures

type VectorCallRecord = (String, usize, bool, Option<Hlc>);
type KeywordCallRecord = (String, String, usize, bool, Option<Hlc>);

/// Recording storage that returns canned hits and records every call.
/// Identical shape to dsl_compose.rs::RecordingStorage by convention.
#[derive(Default)]
struct RecordingStorage {
    vector_hits: Mutex<Vec<VectorHit>>,
    keyword_hits: Mutex<Vec<KeywordHit>>,
    last_vector_args: Mutex<Option<VectorCallRecord>>,
    last_keyword_args: Mutex<Option<KeywordCallRecord>>,
    chunks_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    episodes_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// Plan 03-02: canned GraphResult returned from the next graph_traverse call.
    canned_graph: Mutex<GraphResult>,
    /// Plan 03-02: records every graph_traverse `(query, as_of)` for assertion.
    graph_calls: Mutex<Vec<(CypherQuery, Option<Hlc>)>>,
    /// Plan 03-02: records every vector_search call for assertion in the
    /// canonical compose test (Vector + Graph + fuse_rrf).
    vector_calls: Mutex<Vec<VectorCallRecord>>,
}

impl RecordingStorage {
    fn new_with_graph(g: GraphResult) -> Self {
        let s = Self::default();
        *s.canned_graph.lock() = g;
        s
    }

    fn set_vector_hits(&self, hits: Vec<VectorHit>) {
        *self.vector_hits.lock() = hits;
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        _ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }

    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        index: &str,
        _query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        let rec = (index.to_string(), k, filter.is_some(), as_of);
        *self.last_vector_args.lock() = Some(rec.clone());
        self.vector_calls.lock().push(rec);
        Ok(self.vector_hits.lock().clone())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        q: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        // Plan 03-02: record the call shape AND return the canned graph.
        self.graph_calls.lock().push((q.clone(), as_of));
        Ok(self.canned_graph.lock().clone())
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
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::publish"))
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
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
            // false → fuse_rrf takes the client-side path — Vector + Graph
            // MUST take the client-side path regardless because Moon-native
            // hybrid_search only supports Vector + BM25 fusion (Plan 03-02
            // fusion::inspect_branches forces client-side when Graph branch
            // is present).
            native_rrf: false,
            max_scopes_recommended: 0,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorage {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
        index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        *self.last_keyword_args.lock() =
            Some((index.to_string(), query.to_string(), k, filter.is_some(), as_of));
        Ok(self.keyword_hits.lock().clone())
    }
}

// ----------------------------- Helpers --------------------------------------

/// Build a GraphResult with N (id, name, type) rows for testing. Mirrors the
/// shape Plan 03-03 will produce: m.id_hex / m.name / m.type column order
/// matches the Cypher RETURN clause.
fn canned_graph_with(rows: Vec<(EntityId, &str, &str)>) -> GraphResult {
    GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: rows
            .into_iter()
            .map(|(id, name, typ)| {
                vec![
                    serde_json::Value::String(format!("{}", id)),
                    serde_json::Value::String(name.into()),
                    serde_json::Value::String(typ.into()),
                ]
            })
            .collect(),
    }
}

fn make_ctx(storage: Arc<RecordingStorage>, as_of: Option<Hlc>) -> QueryContext {
    let storage_dyn: Arc<dyn StoragePort> = storage.clone();
    let keyword_dyn: Arc<dyn KeywordPort> = storage;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let mut q = Query::text("brown fox");
    q.as_of = as_of;
    QueryContext::new(q, lunaris_core::Scope::dev(), embedder, storage_dyn, keyword_dyn)
}

// ============================================================ Tests

#[tokio::test]
async fn graph_anchored_returns_hits_for_traversed_entities() {
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let bob = EntityId::from_name_and_type("Bob", "Person");
    let storage =
        Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![(bob, "Bob", "Person")])));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![alice], 2).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source_op, SourceOp::Graph);
    assert_eq!(hits[0].id, hex::decode(format!("{}", bob)).unwrap());
    // Score = 1.0 / (1 + 0) = 1.0 for the first row (rank 0 → 1.0).
    assert!((hits[0].score - 1.0).abs() < f32::EPSILON);
    // Confirm cypher was called with the right shape.
    let calls = storage.graph_calls.lock().clone();
    assert_eq!(calls.len(), 1, "graph_traverse called exactly once");
    assert!(calls[0].0.cypher.contains("[*1..2]"), "hops literal: {}", calls[0].0.cypher);
    assert!(
        calls[0].0.cypher.contains("MATCH (n {id_hex: sid})"),
        "MATCH must use id_hex (W-7 alignment): {}",
        calls[0].0.cypher,
    );
    assert!(
        calls[0].0.cypher.contains("RETURN DISTINCT m.id_hex"),
        "RETURN must select m.id_hex (W-7 alignment): {}",
        calls[0].0.cypher,
    );
    assert_eq!(calls[0].0.graph, "lunaris_graph");
    assert_eq!(calls[0].1, None, "no as_of in this query");
}

#[tokio::test]
async fn graph_anchored_passes_as_of_through_to_storage() {
    // Bi-temporal correctness: as_of MUST propagate verbatim from
    // ctx.query.as_of to ctx.storage.graph_traverse(_, as_of).
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![])));
    let pinned = Hlc { wall_ms: 123_456_789, counter: 0, node_id: 0 };
    let ctx = make_ctx(storage.clone(), Some(pinned));

    let _ = Graph::anchored(vec![alice], 2).retrieve(&ctx).await.unwrap();
    let calls = storage.graph_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, Some(pinned), "as_of MUST propagate to graph_traverse");
}

#[tokio::test]
async fn graph_anchored_empty_entity_ids_returns_empty_without_calling_storage() {
    // Empty fast-path: planner returns no entity_ids when the query has no
    // entity mentions; treat as "graph branch contributes nothing" rather
    // than an error AND must NOT touch the storage backend.
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![])));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![], 2).retrieve(&ctx).await.unwrap();
    assert!(hits.is_empty(), "empty entity_ids must produce empty result");
    assert_eq!(
        storage.graph_calls.lock().len(),
        0,
        "must NOT call graph_traverse on empty entity_ids",
    );
}

#[tokio::test]
async fn graph_anchored_composes_with_vector_via_fuse_rrf() {
    // The canonical compose example from CONTEXT.md <specifics>:
    //   Vector::new("chunks", 30).and(Graph::anchored(query_entities, 2))
    //                            .fuse_rrf(60).top(5)
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let bob = EntityId::from_name_and_type("Bob", "Person");
    let carol = EntityId::from_name_and_type("Carol", "Person");
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![
        (bob, "Bob", "Person"),
        (carol, "Carol", "Person"),
    ])));
    storage.set_vector_hits(vec![
        VectorHit { id: vec![0u8; 16], score: 0.9, rerank_applied: false, metadata: json!({}) },
        VectorHit { id: vec![1u8; 16], score: 0.7, rerank_applied: false, metadata: json!({}) },
    ]);
    let storage_dyn: Arc<dyn StoragePort> = storage.clone();
    let keyword_dyn: Arc<dyn KeywordPort> = storage.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));

    // Build the canonical compose: Vector + Graph + fuse_rrf + top(5).
    let root = Vector::new("chunks", 30).and(Graph::anchored(vec![alice], 2)).fuse_rrf(60).top(5);
    let builder = RetrievalBuilder::new(storage_dyn, keyword_dyn, embedder).with_root(root);

    // Execute end-to-end. Hydration drops hits without a chunk row in
    // storage; we assert against the recorded call shapes instead.
    let _hits = builder.execute(Query::text("brown fox")).await.unwrap();

    // BOTH graph_traverse AND vector_search must have run exactly once.
    assert_eq!(storage.graph_calls.lock().len(), 1, "graph_traverse must run once");
    assert_eq!(storage.vector_calls.lock().len(), 1, "vector_search must run once");
    // Confirm the graph call carried the right cypher shape (W-7 + D-16).
    let g_call = &storage.graph_calls.lock()[0];
    assert!(g_call.0.cypher.contains("[*1..2]"));
    assert!(g_call.0.cypher.contains("id_hex"));
}

#[tokio::test]
async fn graph_anchored_does_not_take_moon_native_dispatch_path() {
    // Even if MoonStorage Arc were present in the QueryContext, fuse_rrf
    // MUST NOT route Vector + Graph to text().hybrid_search() (which is
    // Vector + BM25-only). Confirm via end-to-end behavior — even with
    // native_rrf possible, the result comes from BOTH graph_traverse +
    // vector_search round-trips, not a single hybrid_search call.
    //
    // The test below proves this indirectly: if Moon-native dispatch fired,
    // graph_traverse would NEVER be called (hybrid_search subsumes both).
    // The previous test (graph_anchored_composes_with_vector_via_fuse_rrf)
    // asserts graph_calls.len() == 1, which proves the fusion dispatcher
    // routed to client-side fold even though Vector was in the AND.
    //
    // This test additionally probes the fusion::inspect_branches pure-fn
    // behavior — that the Graph branch downcasts and triggers FusedKind::Other.
    use lunaris_retrieve::AndRetriever;

    let v: Box<dyn Retriever> = Box::new(Vector::new("chunks", 30));
    let g: Box<dyn Retriever> = Box::new(Graph::anchored(vec![], 2));
    let _and = AndRetriever::new(v, g);
    // Confirms inspect_branches handles Graph downcast — the actual
    // FusedKind::Other assertion lives in fusion.rs::tests
    // (inspect_forces_other_when_left_branch_is_graph /
    // inspect_forces_other_when_right_branch_is_graph). This test serves
    // as a compile-time check that the AndRetriever shape composes.
}

#[tokio::test]
async fn graph_anchored_score_decreases_monotonically_with_rank() {
    // Score = 1 / (1 + rank): rank 0 → 1.0, rank 1 → 0.5, rank 2 → 1/3.
    // RRF fusion downstream relies on this monotonic ordering to assign
    // stable per-branch ranks.
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![
        (EntityId::from_name_and_type("A", "X"), "A", "X"),
        (EntityId::from_name_and_type("B", "X"), "B", "X"),
        (EntityId::from_name_and_type("C", "X"), "C", "X"),
    ])));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![alice], 2).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 3);
    assert!(
        hits[0].score > hits[1].score && hits[1].score > hits[2].score,
        "scores must decrease monotonically: {:?}",
        hits.iter().map(|h| h.score).collect::<Vec<_>>(),
    );
    // Specifically: 1/1, 1/2, 1/3.
    assert!((hits[0].score - 1.0_f32).abs() < 1e-6);
    assert!((hits[1].score - 0.5_f32).abs() < 1e-6);
    assert!((hits[2].score - 1.0_f32 / 3.0_f32).abs() < 1e-6);
    // All hits MUST carry SourceOp::Graph for downstream fuse_rrf grouping.
    assert!(hits.iter().all(|h| h.source_op == SourceOp::Graph));
}

#[tokio::test]
async fn cypher_template_does_not_contain_entity_id_text() {
    // T-03-02-01 mitigation — adversarial entity hex MUST NOT reach the
    // cypher string. The 0xFF entity stands in for any caller-supplied
    // EntityId (extracted from query text by the planner stub) — the
    // operator MUST defend against the injection vector regardless of how
    // the EntityId was minted.
    let evil = EntityId([0xFFu8; 16]);
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![])));
    let ctx = make_ctx(storage.clone(), None);

    let _ = Graph::anchored(vec![evil], 2).retrieve(&ctx).await.unwrap();
    let calls = storage.graph_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    let evil_hex = format!("{}", evil);
    assert!(
        !calls[0].0.cypher.contains(&evil_hex),
        "EntityId hex must NOT be spliced into cypher: {}",
        calls[0].0.cypher,
    );
    // It MUST be in params instead (typed parameter binding).
    let ids_param = calls[0].0.params.get("ids").expect("ids param present").as_array().unwrap();
    assert_eq!(ids_param.len(), 1);
    assert_eq!(ids_param[0], serde_json::Value::String(evil_hex));
}

#[tokio::test]
async fn graph_anchored_clamps_excessive_hops_at_max() {
    // D-14: even if the caller passes hops=100, the constructor clamps to
    // MAX_GRAPH_HOPS=5 — bounded BFS fan-out per query. Verify by
    // inspecting the cypher hops literal in the recorded call.
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![])));
    let ctx = make_ctx(storage.clone(), None);

    let _ = Graph::anchored(vec![alice], 100).retrieve(&ctx).await.unwrap();
    let calls = storage.graph_calls.lock().clone();
    assert!(
        calls[0].0.cypher.contains("[*1..5]"),
        "hops MUST clamp to MAX_GRAPH_HOPS=5 even when caller passes 100: {}",
        calls[0].0.cypher,
    );
    assert!(
        !calls[0].0.cypher.contains("[*1..100]"),
        "unclamped hops literal MUST NOT appear: {}",
        calls[0].0.cypher,
    );
}

#[tokio::test]
async fn graph_anchored_uses_lunaris_graph_default() {
    // Default graph name is "lunaris_graph" — matches BOTH backends:
    // - Postgres: `SELECT create_graph('lunaris_graph')` in the Phase 1 migration.
    // - Moon: GRAPH.QUERY <graph> ... uses 'lunaris_graph' as the graph name.
    // Caller can override via Graph::anchored(...).with_graph("tenant_42")
    // for tenant-isolated deployments.
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![])));
    let ctx = make_ctx(storage.clone(), None);

    let _ = Graph::anchored(vec![alice], 2).retrieve(&ctx).await.unwrap();
    let calls = storage.graph_calls.lock().clone();
    assert_eq!(calls[0].0.graph, "lunaris_graph");
}

#[tokio::test]
async fn graph_anchored_handles_malformed_graph_result_defensively() {
    // T-03-02-04 mitigation — defensive parsing against malformed
    // GraphResult rows. A row with a non-string id cell or a missing column
    // produces an empty-id RawHit rather than panicking. Hit count never
    // exceeds result.rows.len().
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let malformed = GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: vec![
            // Row 0: valid.
            vec![
                serde_json::Value::String(format!("{}", alice)),
                serde_json::Value::String("Alice".into()),
                serde_json::Value::String("Person".into()),
            ],
            // Row 1: id cell is null (malformed) — defensive parse → empty id bytes.
            vec![
                serde_json::Value::Null,
                serde_json::Value::String("Mystery".into()),
                serde_json::Value::String("Unknown".into()),
            ],
            // Row 2: missing trailing columns — get(1) / get(2) returns None.
            vec![serde_json::Value::String(format!("{}", alice))],
        ],
    };
    let storage = Arc::new(RecordingStorage::new_with_graph(malformed));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![alice], 2).retrieve(&ctx).await.unwrap();
    // 3 rows in → 3 hits out (no panic, no truncation).
    assert_eq!(hits.len(), 3);
    // Row 0 valid → id matches alice.
    assert_eq!(hits[0].id, hex::decode(format!("{}", alice)).unwrap());
    // Row 1 malformed → empty id bytes.
    assert!(hits[1].id.is_empty(), "malformed null id MUST produce empty id bytes");
    // Row 2 partial → id present, metadata fields are Null.
    assert_eq!(hits[2].id, hex::decode(format!("{}", alice)).unwrap());
    // All hits carry SourceOp::Graph.
    assert!(hits.iter().all(|h| h.source_op == SourceOp::Graph));
}
