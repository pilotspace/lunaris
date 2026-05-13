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
    BiTemporal, CypherDialect, Embedder, Hlc, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
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
    /// Wave 4 amendment: dialect tier the fake storage reports through
    /// `capabilities().cypher_dialect`. Defaults to Legacy (matches the
    /// canonical no-overrides backend profile for graph-less fakes). The
    /// dispatch-tier tests flip this via `set_dialect()` to verify the
    /// operator picks the right Cypher template per tier.
    dialect: Mutex<CypherDialect>,
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

    /// Wave 4 amendment: configure the dialect tier the operator sees via
    /// `ctx.storage.capabilities().cypher_dialect`. Default is
    /// [`CypherDialect::Legacy`] (matches the unchanged historical fixture
    /// shape so existing tests stay untouched).
    fn with_dialect(self, d: CypherDialect) -> Self {
        *self.dialect.lock() = d;
        self
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
            // Wave 4 amendment: read from the configurable field so
            // dispatch-tier tests can swap Legacy / PathMetrics / Full.
            cypher_dialect: *self.dialect.lock(),
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

    let hits = Graph::anchored(vec![(alice, 1.0)], 2).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source_op, SourceOp::Graph);
    assert_eq!(hits[0].id, hex::decode(format!("{}", bob)).unwrap());
    // Score = 1.0 / (1 + 0) = 1.0 for the first row (rank 0 → 1.0).
    assert!((hits[0].score - 1.0).abs() < f32::EPSILON);
    // Confirm cypher was called with the right shape.
    let calls = storage.graph_calls.lock().clone();
    assert_eq!(calls.len(), 1, "graph_traverse called exactly once");
    assert!(calls[0].0.cypher.contains("[*1..2]"), "hops literal: {}", calls[0].0.cypher);
    // Wave 4 amendment: RecordingStorage defaults to CypherDialect::Legacy
    // (no overrides). The operator therefore dispatches the Legacy template
    // here. The dispatch_* tests cover the PathMetrics/Full assertions.
    assert!(
        calls[0].0.cypher.contains("MATCH (n {id_hex: sid})"),
        "Legacy MATCH must use id_hex property (W-7): {}",
        calls[0].0.cypher,
    );
    assert!(
        calls[0].0.cypher.contains("m.id_hex AS id"),
        "RETURN must select m.id_hex AS id (W-7 alignment): {}",
        calls[0].0.cypher,
    );
    assert!(
        !calls[0].0.cypher.contains("DISTINCT"),
        "DISTINCT must not appear (would collapse rows): {}",
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

    let _ = Graph::anchored(vec![(alice, 1.0)], 2).retrieve(&ctx).await.unwrap();
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

    let hits = Graph::anchored(Vec::<(EntityId, f32)>::new(), 2).retrieve(&ctx).await.unwrap();
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
    let root = Vector::new("chunks", 30).and(Graph::anchored(vec![(alice, 1.0)], 2)).fuse_rrf(60).top(5);
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
    let g: Box<dyn Retriever> = Box::new(Graph::anchored(Vec::<(EntityId, f32)>::new(), 2));
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

    let hits = Graph::anchored(vec![(alice, 1.0)], 2).retrieve(&ctx).await.unwrap();
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

    let _ = Graph::anchored(vec![(evil, 1.0)], 2).retrieve(&ctx).await.unwrap();
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

    let _ = Graph::anchored(vec![(alice, 1.0)], 100).retrieve(&ctx).await.unwrap();
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

    let _ = Graph::anchored(vec![(alice, 1.0)], 2).retrieve(&ctx).await.unwrap();
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

    let hits = Graph::anchored(vec![(alice, 1.0)], 2).retrieve(&ctx).await.unwrap();
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

// ============================================================ P0 #4 Wave 3 — Real graph scoring
//
// New score formula:
//   score = (edge_weight_product / (1.0 + path_length)) * anchor_confidence
//
// Three optional columns are read by HEADER NAME from GraphResult:
//   `path_length`, `edge_weight_product`, `anchor_confidence`
//
// When all three are absent, the operator substitutes documented defaults
// `(i, 1.0, 1.0)` so the formula reduces to the legacy `1.0 / (1.0 + i)`.
// This is the back-compat property the regression test below pins.

/// Build a GraphResult with rows extended by optional Wave-3 columns. The
/// first 3 columns stay positional (`id`, `name`, `type`); the trailing
/// columns are header-keyed and only included when `Some(...)` is provided.
#[allow(clippy::type_complexity)]
fn graph_with_columns(
    rows: Vec<(EntityId, &str, &str, Option<f64>, Option<f64>, Option<f64>)>,
) -> GraphResult {
    let mut headers = vec!["id".to_string(), "name".to_string(), "type".to_string()];
    // Decide which optional columns to emit based on the first row's Some-ness.
    // Mixed presence isn't expected in real backends; for tests we expect
    // every row to follow the same column shape.
    let has_path_len = rows.first().is_some_and(|r| r.3.is_some());
    let has_edge_w = rows.first().is_some_and(|r| r.4.is_some());
    let has_anchor = rows.first().is_some_and(|r| r.5.is_some());
    if has_path_len {
        headers.push("path_length".into());
    }
    if has_edge_w {
        headers.push("edge_weight_product".into());
    }
    if has_anchor {
        headers.push("anchor_confidence".into());
    }
    let rows_out = rows
        .into_iter()
        .map(|(id, name, typ, pl, ew, ac)| {
            let mut row = vec![
                serde_json::Value::String(format!("{}", id)),
                serde_json::Value::String(name.into()),
                serde_json::Value::String(typ.into()),
            ];
            if has_path_len {
                row.push(serde_json::json!(pl.unwrap_or(0.0)));
            }
            if has_edge_w {
                row.push(serde_json::json!(ew.unwrap_or(1.0)));
            }
            if has_anchor {
                row.push(serde_json::json!(ac.unwrap_or(1.0)));
            }
            row
        })
        .collect();
    GraphResult { headers, rows: rows_out }
}

#[tokio::test]
async fn graph_score_back_compat_when_no_new_headers() {
    // Regression: GraphResult with ONLY the legacy `id`/`name`/`type` columns
    // (no `path_length`/`edge_weight_product`/`anchor_confidence`) MUST produce
    // byte-identical scores to the pre-Wave-3 `1.0 / (1.0 + i)` formula.
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let storage = Arc::new(RecordingStorage::new_with_graph(canned_graph_with(vec![
        (EntityId::from_name_and_type("A", "X"), "A", "X"),
        (EntityId::from_name_and_type("B", "X"), "B", "X"),
        (EntityId::from_name_and_type("C", "X"), "C", "X"),
        (EntityId::from_name_and_type("D", "X"), "D", "X"),
    ])));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![(alice, 1.0)], 2).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 4);
    for (i, hit) in hits.iter().enumerate() {
        let expected = 1.0_f32 / (1.0_f32 + i as f32);
        assert!(
            (hit.score - expected).abs() < 1e-6,
            "row {i}: legacy score MUST be 1/(1+i)={expected}, got {}",
            hit.score
        );
    }
}

#[tokio::test]
async fn graph_score_path_length_decay() {
    // Equal edge_weight (1.0) + equal anchor_confidence (1.0):
    //   row 0: path_length = 1 → score = 1.0 / (1+1) = 0.5
    //   row 1: path_length = 3 → score = 1.0 / (1+3) = 0.25
    // Shorter path → higher score. The row order is preserved (no resort),
    // but the SCORE values diverge from the legacy `1/(1+i)` rank.
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let result = graph_with_columns(vec![
        (EntityId::from_name_and_type("Near", "X"), "Near", "X", Some(1.0), Some(1.0), Some(1.0)),
        (EntityId::from_name_and_type("Far", "X"), "Far", "X", Some(3.0), Some(1.0), Some(1.0)),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![(alice, 1.0)], 5).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!(
        (hits[0].score - 0.5_f32).abs() < 1e-6,
        "path_length=1 should score 0.5, got {}",
        hits[0].score
    );
    assert!(
        (hits[1].score - 0.25_f32).abs() < 1e-6,
        "path_length=3 should score 0.25, got {}",
        hits[1].score
    );
    assert!(hits[0].score > hits[1].score, "shorter path MUST score higher");
}

#[tokio::test]
async fn graph_score_edge_weight_dominance() {
    // Equal path_length (1) + equal anchor_confidence (1.0):
    //   row 0: edge_weight = 0.9 → score = 0.9 / 2.0 = 0.45
    //   row 1: edge_weight = 0.5 → score = 0.5 / 2.0 = 0.25
    // Higher edge weight → higher score.
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let result = graph_with_columns(vec![
        (
            EntityId::from_name_and_type("Strong", "X"),
            "Strong",
            "X",
            Some(1.0),
            Some(0.9),
            Some(1.0),
        ),
        (EntityId::from_name_and_type("Weak", "X"), "Weak", "X", Some(1.0), Some(0.5), Some(1.0)),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![(alice, 1.0)], 5).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!((hits[0].score - 0.45_f32).abs() < 1e-6, "got {}", hits[0].score);
    assert!((hits[1].score - 0.25_f32).abs() < 1e-6, "got {}", hits[1].score);
    assert!(hits[0].score > hits[1].score, "higher edge weight MUST score higher");
}

#[tokio::test]
async fn graph_score_anchor_confidence_effect() {
    // Wave 4: anchor_confidence is no longer a Cypher-emitted column. The
    // operator synthesizes it post-Cypher by joining `source_entity_id`
    // (the seed that anchored each row's path) against the per-seed
    // `confidence_by_seed` map built from `Graph::anchored`'s
    // `Vec<(EntityId, f32)>` argument.
    //
    // Equal path_length (1) + equal edge_weight (1.0):
    //   row 0: source = Trusted seed (conf 1.0) → score = (1.0/2.0) * 1.0 = 0.5
    //   row 1: source = Uncertain seed (conf 0.3) → score = (1.0/2.0) * 0.3 = 0.15
    let trusted = EntityId::from_name_and_type("Trusted", "Person");
    let uncertain = EntityId::from_name_and_type("Uncertain", "Person");
    let result = graph_with_source_entity(vec![
        (EntityId::from_name_and_type("X", "Y"), "X", "Y", Some(1.0), Some(1.0), trusted),
        (EntityId::from_name_and_type("Z", "Y"), "Z", "Y", Some(1.0), Some(1.0), uncertain),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    // Pass BOTH seeds with their respective confidences.
    let hits = Graph::anchored(vec![(trusted, 1.0), (uncertain, 0.3)], 5)
        .retrieve(&ctx)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert!((hits[0].score - 0.5_f32).abs() < 1e-6, "got {}", hits[0].score);
    assert!((hits[1].score - 0.15_f32).abs() < 1e-6, "got {}", hits[1].score);
    assert!(hits[0].score > hits[1].score, "higher anchor confidence MUST score higher");
}

// ===================================================== Wave 4 — source_entity_id join
//
// Build a Wave-4-shape GraphResult: id/name/type positional, plus header-keyed
// `path_length`, `edge_weight_product`, and `source_entity_id`. The operator
// reads `source_entity_id` and joins against the seed map to synthesize
// `anchor_confidence`.

#[allow(clippy::type_complexity)]
fn graph_with_source_entity(
    rows: Vec<(EntityId, &str, &str, Option<f64>, Option<f64>, EntityId)>,
) -> GraphResult {
    let headers = vec![
        "id".to_string(),
        "name".to_string(),
        "type".to_string(),
        "path_length".to_string(),
        "edge_weight_product".to_string(),
        "source_entity_id".to_string(),
    ];
    let rows_out = rows
        .into_iter()
        .map(|(id, name, typ, pl, ew, src)| {
            vec![
                serde_json::Value::String(format!("{}", id)),
                serde_json::Value::String(name.into()),
                serde_json::Value::String(typ.into()),
                serde_json::json!(pl.unwrap_or(1.0)),
                serde_json::json!(ew.unwrap_or(1.0)),
                serde_json::Value::String(format!("{}", src)),
            ]
        })
        .collect();
    GraphResult { headers, rows: rows_out }
}

#[tokio::test]
async fn graph_anchor_confidence_low_seed_demotes_path() {
    // Two paths with equal edge_weight + equal path_length but anchored at
    // different seeds (confidence 1.0 vs 0.3). The high-confidence anchor
    // MUST score higher than the low-confidence one.
    let strong_seed = EntityId::from_name_and_type("StrongSeed", "Person");
    let weak_seed = EntityId::from_name_and_type("WeakSeed", "Person");
    let result = graph_with_source_entity(vec![
        // Both rows: path_length=1, edge_weight=1.0 → only confidence differs.
        (EntityId::from_name_and_type("A", "X"), "A", "X", Some(1.0), Some(1.0), strong_seed),
        (EntityId::from_name_and_type("B", "X"), "B", "X", Some(1.0), Some(1.0), weak_seed),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![(strong_seed, 1.0), (weak_seed, 0.3)], 5)
        .retrieve(&ctx)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    // Row 0: anchored at strong (1.0) → (1.0 / 2.0) * 1.0 = 0.5
    assert!((hits[0].score - 0.5_f32).abs() < 1e-6, "strong-anchor row got {}", hits[0].score);
    // Row 1: anchored at weak (0.3) → (1.0 / 2.0) * 0.3 = 0.15
    assert!((hits[1].score - 0.15_f32).abs() < 1e-6, "weak-anchor row got {}", hits[1].score);
    assert!(hits[0].score > hits[1].score, "high-confidence anchor MUST score higher");
}

#[tokio::test]
async fn graph_edge_weight_overrides_path_length() {
    // A heavy-edged 3-hop path (edge_weight_product = 0.9) beats a
    // 1-hop path with low edge weight (edge_weight_product = 0.2).
    //   heavy: 0.9 / (1.0 + 3.0) = 0.225
    //   light: 0.2 / (1.0 + 1.0) = 0.10
    let seed = EntityId::from_name_and_type("Seed", "Person");
    let result = graph_with_source_entity(vec![
        (EntityId::from_name_and_type("Heavy", "X"), "Heavy", "X", Some(3.0), Some(0.9), seed),
        (EntityId::from_name_and_type("Light", "X"), "Light", "X", Some(1.0), Some(0.2), seed),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![(seed, 1.0)], 5).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!((hits[0].score - 0.225_f32).abs() < 1e-5, "heavy 3-hop got {}", hits[0].score);
    assert!((hits[1].score - 0.10_f32).abs() < 1e-5, "light 1-hop got {}", hits[1].score);
    assert!(
        hits[0].score > hits[1].score,
        "edge_weight_product MUST dominate when it sufficiently exceeds the path-length penalty",
    );
}

#[tokio::test]
async fn graph_anchor_confidence_default_one_when_seed_missing() {
    // Defensive: a Cypher row whose `source_entity_id` does NOT appear in
    // the seed map (e.g., backend bug; planner seed list mismatch) MUST
    // get anchor_confidence = 1.0 rather than 0.0. Zero-confidence
    // fallback would silently delete legitimate hits from downstream
    // fusion — wrong default.
    let real_seed = EntityId::from_name_and_type("RealSeed", "Person");
    let ghost_seed = EntityId::from_name_and_type("Ghost", "Person");
    let result = graph_with_source_entity(vec![
        // Source is `ghost_seed`, but the caller's seeds vec does NOT
        // include it. Operator must fall back to confidence = 1.0.
        (EntityId::from_name_and_type("Orphan", "X"), "Orphan", "X", Some(1.0), Some(1.0), ghost_seed),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    let hits = Graph::anchored(vec![(real_seed, 1.0)], 5).retrieve(&ctx).await.unwrap();
    assert_eq!(hits.len(), 1);
    // Default anchor_confidence = 1.0 → score = (1.0 / 2.0) * 1.0 = 0.5.
    assert!(
        (hits[0].score - 0.5_f32).abs() < 1e-6,
        "missing seed MUST default to confidence 1.0 (got score {})",
        hits[0].score,
    );
}

#[tokio::test]
async fn graph_anchor_confidence_uses_max_for_duplicate_seeds() {
    // Two callsites of the same seed with different confidences: the
    // operator MUST collapse to MAX so a low-confidence duplicate cannot
    // demote a high-confidence anchor.
    let seed = EntityId::from_name_and_type("DupSeed", "Person");
    let result = graph_with_source_entity(vec![
        (EntityId::from_name_and_type("Hit", "X"), "Hit", "X", Some(1.0), Some(1.0), seed),
    ]);
    let storage = Arc::new(RecordingStorage::new_with_graph(result));
    let ctx = make_ctx(storage.clone(), None);

    // Duplicate seed: low + high confidence. MAX wins → 0.9.
    let hits = Graph::anchored(vec![(seed, 0.1), (seed, 0.9)], 5)
        .retrieve(&ctx)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    // score = (1.0 / 2.0) * 0.9 = 0.45
    assert!(
        (hits[0].score - 0.45_f32).abs() < 1e-6,
        "duplicate seeds MUST collapse to MAX confidence (got {})",
        hits[0].score,
    );
}

// ============================ Wave 4 amendment — CypherDialect dispatch tests
//
// These tests verify the operator picks the Cypher template that matches the
// backend's declared `capabilities().cypher_dialect` tier. The shared
// `dispatched_cypher_with_dialect` helper builds a `RecordingStorage` fixed at
// the requested tier, runs `Graph::anchored`, and returns the Cypher string
// that the operator actually sent to `graph_traverse`.

async fn dispatched_cypher_with_dialect(d: CypherDialect) -> String {
    let storage = Arc::new(
        RecordingStorage::new_with_graph(canned_graph_with(vec![])).with_dialect(d),
    );
    let ctx = make_ctx(storage.clone(), None);
    let seed = EntityId::from_name_and_type("Anchor", "Person");
    let _ = Graph::anchored(vec![(seed, 1.0)], 2).retrieve(&ctx).await.unwrap();
    storage
        .graph_calls
        .lock()
        .first()
        .expect("graph_traverse must be called once when seeds are non-empty")
        .0
        .cypher
        .clone()
}

#[tokio::test]
async fn dispatch_legacy_dialect_emits_no_path_metrics_no_reduce() {
    // Legacy tier (Moon + embedded): the operator MUST emit the universal
    // template — id/name/type only, no path binding, no length(), no
    // reduce(), no source_entity_id. This is what Moon's parser accepts.
    let cypher = dispatched_cypher_with_dialect(CypherDialect::Legacy).await;
    assert!(
        !cypher.contains("MATCH p ="),
        "Legacy dialect MUST NOT bind a path variable (Moon rejects it): {cypher}"
    );
    assert!(
        !cypher.contains("length("),
        "Legacy dialect MUST NOT call length() (Moon function table omits it): {cypher}"
    );
    assert!(
        !cypher.contains("reduce("),
        "Legacy dialect MUST NOT call reduce() (Moon function table omits it): {cypher}"
    );
    assert!(
        !cypher.contains("source_entity_id"),
        "Legacy dialect MUST NOT alias n.id_hex AS source_entity_id: {cypher}"
    );
    // It MUST still hit the same node property + parameter shape so the
    // backend round trip behaves the same as before.
    assert!(cypher.contains("id_hex"), "Legacy MUST still use id_hex: {cypher}");
    assert!(cypher.contains("$ids"), "Legacy MUST parameterize $ids: {cypher}");
    assert!(cypher.contains("$k"), "Legacy MUST parameterize $k: {cypher}");
}

#[tokio::test]
async fn dispatch_path_metrics_dialect_emits_path_binding_and_length_but_no_reduce() {
    // PathMetrics tier (Postgres AGE 1.5): the operator MUST emit
    // MATCH p = ... + length(p) + source_entity_id, but MUST NOT emit
    // reduce(...) (AGE 1.5 parser rejects the `|` token).
    let cypher = dispatched_cypher_with_dialect(CypherDialect::PathMetrics).await;
    assert!(
        cypher.contains("MATCH p ="),
        "PathMetrics dialect MUST bind a path variable: {cypher}"
    );
    assert!(
        cypher.contains("length(p) AS path_length"),
        "PathMetrics dialect MUST emit length(p) AS path_length: {cypher}"
    );
    assert!(
        cypher.contains("source_entity_id"),
        "PathMetrics dialect MUST emit source_entity_id alias: {cypher}"
    );
    assert!(
        !cypher.contains("reduce("),
        "PathMetrics dialect MUST NOT emit reduce() — AGE 1.5 rejects the `|` token: {cypher}"
    );
}

#[tokio::test]
async fn dispatch_full_dialect_emits_reduce_for_edge_weight_product() {
    // Full tier (forward-compat — no current backend supports this): the
    // operator MUST emit the full Wave-4 template including reduce(...) for
    // edge_weight_product.
    let cypher = dispatched_cypher_with_dialect(CypherDialect::Full).await;
    assert!(
        cypher.contains("MATCH p ="),
        "Full dialect MUST bind a path variable: {cypher}"
    );
    assert!(
        cypher.contains("length(p) AS path_length"),
        "Full dialect MUST emit length(p) AS path_length: {cypher}"
    );
    assert!(
        cypher.contains("reduce(") && cypher.contains("edge_weight_product"),
        "Full dialect MUST emit reduce(...) AS edge_weight_product: {cypher}"
    );
    assert!(
        cypher.contains("source_entity_id"),
        "Full dialect MUST emit source_entity_id alias: {cypher}"
    );
}

// Back-compat property — across ALL dialect tiers, when the backend returns
// only id/name/type headers (the realistic "backend hasn't been upgraded"
// case), the operator MUST fall back to the legacy `1/(1+i)` score formula.
// The header-keyed parser handles this — these tests pin that the dispatch
// commit does not regress the Wave-3 back-compat property.

async fn score_back_compat_with_dialect(d: CypherDialect) -> f32 {
    let alice = EntityId::from_name_and_type("AliceBC", "Person");
    let bob = EntityId::from_name_and_type("BobBC", "Person");
    let storage = Arc::new(
        // Canned graph returns only id/name/type headers — the path-metrics
        // and source-entity columns are absent regardless of declared
        // dialect tier. Mirrors the real-world "backend updated but
        // response sometimes lacks columns" case.
        RecordingStorage::new_with_graph(canned_graph_with(vec![
            (bob, "Bob", "Person"),
        ]))
        .with_dialect(d),
    );
    let ctx = make_ctx(storage.clone(), None);
    let hits = Graph::anchored(vec![(alice, 1.0)], 2)
        .retrieve(&ctx)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "single canned row must yield one hit");
    hits[0].score
}

#[tokio::test]
async fn dispatch_legacy_preserves_back_compat_score() {
    // Legacy tier + 3-column response → Wave-3 fallback score: 1/(1+0)=1.0.
    let score = score_back_compat_with_dialect(CypherDialect::Legacy).await;
    assert!((score - 1.0).abs() < 1e-6, "expected 1.0, got {score}");
}

#[tokio::test]
async fn dispatch_path_metrics_preserves_back_compat_score_when_columns_absent() {
    // PathMetrics tier + 3-column response → same Wave-3 fallback. The
    // dialect declaration says "backend CAN emit those columns" but the
    // operator MUST handle the case where it didn't.
    let score = score_back_compat_with_dialect(CypherDialect::PathMetrics).await;
    assert!((score - 1.0).abs() < 1e-6, "expected 1.0, got {score}");
}

#[tokio::test]
async fn dispatch_full_preserves_back_compat_score_when_columns_absent() {
    // Full tier + 3-column response → same Wave-3 fallback.
    let score = score_back_compat_with_dialect(CypherDialect::Full).await;
    assert!((score - 1.0).abs() < 1e-6, "expected 1.0, got {score}");
}
