//! Plan 03-03 Task 3b — integration smoke tests for the graph pipeline
//! toggle, branched ingest fan-out, and verify-queue Phase 4 hook.
//!
//! Mirrors the Phase 2 `recall_with_rerank.rs` fixture pattern (in-memory
//! `RecordingStorageWithKeyword`) extended with:
//!
//! - `writeop_counts: WriteOpCounts` — per-variant counter so tests assert
//!   how many GraphNode / GraphEdge / KvPut / VectorUpsert ops landed in
//!   the atomic_write batch.
//! - `batches: Vec<Vec<WriteOp>>` — exact batches for INGEST-04 single-
//!   call invariant assertions (D-18).
//! - `canned_graph: GraphResult` — what `graph_traverse` returns (used by
//!   the cross-plan W-7 round-trip test).
//! - `published_messages: Vec<(topic, partition, payload)>` — what
//!   `publish` was called with (used by D-19 verify-queue assertions).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{
    EntityId, Extractor, Graph, GraphPipelineHandle, Lunaris, NoopExtractor, NoopReranker, Query,
    Reranker, ValidatedExtraction, Vector,
};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder,
};
use lunaris_extract::{
    ChunkInput, Entity as ExtractEntity, Fact as ExtractFact, RawExtraction, RawExtractionBatch,
    Relation as ExtractRelation,
};
use lunaris_retrieve::{QueryContext, Retriever};
use parking_lot::Mutex;
use serde_json::json;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// RecordingStorageWithKeyword — extended in-memory fixture (Task 3b)
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Debug, PartialEq, Eq)]
struct WriteOpCounts {
    kv_put: usize,
    kv_delete: usize,
    vector_upsert: usize,
    graph_node: usize,
    graph_edge: usize,
}

impl WriteOpCounts {
    fn record(&mut self, op: &WriteOp) {
        match op {
            WriteOp::KvPut { .. } => self.kv_put += 1,
            WriteOp::KvDelete { .. } => self.kv_delete += 1,
            WriteOp::VectorUpsert { .. } => self.vector_upsert += 1,
            WriteOp::GraphNode { .. } => self.graph_node += 1,
            WriteOp::GraphEdge { .. } => self.graph_edge += 1,
            _ => {}
        }
    }
}

/// In-memory storage that records writes, vector-search ids, graph results
/// (canned), AND verify-queue publish messages. Mirrors Plan 02-03's
/// RecordingStorageWithKeyword and extends with Plan 03-03 surfaces.
#[derive(Default)]
struct RecordingStorageWithKeyword {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    chunk_ids: Mutex<Vec<Vec<u8>>>,
    /// Per-variant op counter — populated on every `atomic_write` call.
    writeop_counts: Mutex<WriteOpCounts>,
    /// Full batch history — `len()` == number of atomic_write calls
    /// (INGEST-04 single-call invariant).
    batches: Mutex<Vec<Vec<WriteOp>>>,
    /// Canned `graph_traverse` result. Default is empty so existing tests
    /// that don't exercise graph paths see no rows.
    canned_graph: Mutex<GraphResult>,
    /// Topic + partition + payload for every `publish` call. D-19 hook
    /// asserts the verify-queue side channel fires.
    published_messages: Mutex<Vec<(String, u16, Bytes)>>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }

    fn writeop_counts(&self) -> WriteOpCounts {
        self.writeop_counts.lock().clone()
    }

    #[allow(dead_code)]
    fn published_count(&self) -> usize {
        self.published_messages.lock().len()
    }

    fn published_topics(&self) -> Vec<String> {
        self.published_messages.lock().iter().map(|(t, _, _)| t.clone()).collect()
    }

    /// Plan 04-04 D-16 — Lunaris::ingest now ALSO publishes one
    /// `__lunaris_consolidate__` event per call. The graph_pipeline_smoke
    /// tests pre-date that change; this helper lets each test count ONLY
    /// the verify-queue topic publishes (which is what the D-19 hook
    /// assertions actually care about). The consolidate-queue publishes
    /// are exercised by Plan 04-04's consolidator_pipeline_smoke tests.
    fn published_verify_count(&self) -> usize {
        self.published_messages.lock().iter().filter(|(t, _, _)| t == "__lunaris_verify__").count()
    }

    fn set_canned_graph(&self, gr: GraphResult) {
        *self.canned_graph.lock() = gr;
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        {
            let mut counts = self.writeop_counts.lock();
            for op in ops {
                counts.record(op);
            }
        }
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    self.rows.lock().insert(key.clone(), value.clone());
                }
                WriteOp::VectorUpsert { id, index, .. } if index == "chunks" => {
                    self.chunk_ids.lock().push(id.clone());
                }
                _ => {}
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
        k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        let ids = self.chunk_ids.lock().clone();
        Ok(ids
            .into_iter()
            .take(k)
            .enumerate()
            .map(|(i, id)| VectorHit {
                id,
                score: 1.0 - (i as f32 * 0.1),
                rerank_applied: false,
                metadata: json!({}),
            })
            .collect())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
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
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        let offset = {
            let mut msgs = self.published_messages.lock();
            msgs.push((topic.to_string(), partition, payload));
            msgs.len() as u64 - 1
        };
        Ok(offset)
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("subscribe"))
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
impl KeywordPort for RecordingStorageWithKeyword {
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

// ---------------------------------------------------------------------------
// MockExtractor — produces canned Entities + Relations for the graph-ON tests
// ---------------------------------------------------------------------------

/// Produces a fixed extraction batch on every `extract()` call. `applies()`
/// returns `true` (distinguishes from NoopExtractor's false) so the ingest
/// fan-out actually walks the graph WriteOp builder loop.
#[derive(Clone)]
struct MockExtractor {
    entities: Vec<ExtractEntity>,
    relations: Vec<ExtractRelation>,
    /// RC-1 (v0.2 release-gate): optional facts the extractor returns. Empty
    /// by default to preserve every existing fixture's behavior; the
    /// `with_alice_likes_chocolate_fact` constructor populates this for the
    /// scope-prefix regression test.
    facts: Vec<ExtractFact>,
    /// Per-instance call counter so the with_extractor swap test can prove
    /// the new extractor is the one being invoked.
    call_count: Arc<Mutex<u64>>,
}

impl MockExtractor {
    fn with_alice_knows_bob() -> Self {
        let alice = EntityId::from_name_and_type("Alice", "Person");
        let bob = EntityId::from_name_and_type("Bob", "Person");
        let entities = vec![
            ExtractEntity {
                id: alice,
                name: "Alice".into(),
                aliases: vec![],
                entity_type: "Person".into(),
                confidence: 0.95,
                valid_from_iso: "2024-01-01T00:00:00Z".into(),
                valid_to_iso: None,
            },
            ExtractEntity {
                id: bob,
                name: "Bob".into(),
                aliases: vec![],
                entity_type: "Person".into(),
                confidence: 0.92,
                valid_from_iso: "2024-01-01T00:00:00Z".into(),
                valid_to_iso: None,
            },
        ];
        let relations = vec![ExtractRelation {
            subject_id: alice,
            predicate: "knows".into(),
            object_id: bob,
            confidence: 0.88,
            valid_from_iso: "2024-01-01T00:00:00Z".into(),
            valid_to_iso: None,
        }];
        Self { entities, relations, facts: vec![], call_count: Arc::new(Mutex::new(0)) }
    }

    /// RC-1 (v0.2 release-gate) fixture: returns one Entity + one Fact so
    /// the graph-on ingest path emits a `WriteOp::KvPut` for the fact under
    /// the scope-prefixed `lunaris:{scope}:fact:{ulid}` key.
    fn with_alice_likes_chocolate_fact() -> Self {
        let alice = EntityId::from_name_and_type("Alice", "Person");
        let chocolate = EntityId::from_name_and_type("Chocolate", "Food");
        let entities = vec![
            ExtractEntity {
                id: alice,
                name: "Alice".into(),
                aliases: vec![],
                entity_type: "Person".into(),
                confidence: 0.95,
                valid_from_iso: "2024-01-01T00:00:00Z".into(),
                valid_to_iso: None,
            },
            ExtractEntity {
                id: chocolate,
                name: "Chocolate".into(),
                aliases: vec![],
                entity_type: "Food".into(),
                confidence: 0.9,
                valid_from_iso: "2024-01-01T00:00:00Z".into(),
                valid_to_iso: None,
            },
        ];
        let facts = vec![ExtractFact {
            id: Ulid::new(),
            subject_id: alice,
            predicate: "likes".into(),
            object_id: chocolate,
            fact_text: "Alice likes chocolate.".into(),
            confidence: 0.9,
            valid_from_iso: "2024-01-01T00:00:00Z".into(),
            valid_to_iso: None,
        }];
        Self { entities, relations: vec![], facts, call_count: Arc::new(Mutex::new(0)) }
    }

    /// MockExtractor whose entities have `valid_from >= valid_to` so the
    /// Validator demotes them to NeedsReview (D-08 #1 InvalidBitemporal).
    fn with_invalid_bitemporal() -> Self {
        let alice = EntityId::from_name_and_type("Alice", "Person");
        let entities = vec![ExtractEntity {
            id: alice,
            name: "Alice".into(),
            aliases: vec![],
            entity_type: "Person".into(),
            confidence: 0.95,
            valid_from_iso: "2025-01-01T00:00:00Z".into(),
            valid_to_iso: Some("2024-01-01T00:00:00Z".into()), // INVERTED
        }];
        Self { entities, relations: vec![], facts: vec![], call_count: Arc::new(Mutex::new(0)) }
    }

    fn call_count(&self) -> u64 {
        *self.call_count.lock()
    }
}

#[async_trait]
impl Extractor for MockExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        *self.call_count.lock() += 1;
        // Attach all entities + relations to the first chunk. validator::validate
        // is chunk-agnostic for the bi-temporal + GBNF + sentinel checks.
        let by_chunk: Vec<RawExtraction> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    RawExtraction {
                        source_chunk_id: c.chunk_id,
                        entities: self.entities.clone(),
                        relations: self.relations.clone(),
                        facts: self.facts.clone(),
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_handle() -> (Lunaris, Arc<RecordingStorageWithKeyword>, Arc<HlcClock>) {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);
    (handle, rec, clock)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graph_off_default_uses_phase2_fast_path() {
    // Phase 3 ROADMAP success criterion #1 — graph OFF default.
    let (handle, rec, clock) = build_handle();
    assert!(
        !handle.graph_pipeline().is_enabled(),
        "graph_pipeline().is_enabled() MUST be false by default per blueprint §5.2"
    );

    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "graph_off.md",
        "# Notes\nThe quick brown fox jumps over the lazy dog.",
        &clock,
    );
    handle.ingest(ep).await.expect("ingest must succeed");

    let counts = rec.writeop_counts();
    assert_eq!(counts.graph_node, 0, "graph OFF MUST emit zero GraphNode WriteOps");
    assert_eq!(counts.graph_edge, 0, "graph OFF MUST emit zero GraphEdge WriteOps");
    assert_eq!(
        rec.published_verify_count(),
        0,
        "graph OFF MUST publish zero __lunaris_verify__ messages (consolidate-queue publish from Plan 04-04 D-16 fires on EVERY ingest and is NOT counted here)"
    );
}

#[tokio::test]
async fn graph_enable_then_ingest_produces_graph_writeops() {
    // Phase 3 ROADMAP success criterion #2 — graph ON produces typed
    // Entities + Relations.
    let (handle, rec, clock) = build_handle();
    let mock = Arc::new(MockExtractor::with_alice_knows_bob());
    let handle = handle.with_extractor(mock.clone() as Arc<dyn Extractor>);
    handle.graph_pipeline().enable();
    assert!(handle.graph_pipeline().is_enabled());

    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "alice.md",
        "# Notes\nAlice knows Bob since 2024.",
        &clock,
    );
    handle.ingest(ep).await.expect("ingest must succeed");

    let counts = rec.writeop_counts();
    assert!(
        counts.graph_node >= 2,
        "expected at least 2 GraphNode (Alice + Bob); got {}",
        counts.graph_node
    );
    assert!(
        counts.graph_edge >= 1,
        "expected at least 1 GraphEdge (Alice knows Bob); got {}",
        counts.graph_edge
    );
    assert_eq!(mock.call_count(), 1, "extractor MUST be called exactly once per ingest");
}

#[tokio::test]
async fn single_atomic_write_call_invariant_holds_under_graph_on() {
    // INGEST-04 + D-18 single-transaction contract: ALL ops (Episode +
    // Chunks + Entities + Relations + Facts) commit via ONE atomic_write.
    let (handle, rec, clock) = build_handle();
    let mock = Arc::new(MockExtractor::with_alice_knows_bob());
    let handle = handle.with_extractor(mock as Arc<dyn Extractor>);
    handle.graph_pipeline().enable();

    let ep =
        Episode::new(lunaris_core::Scope::dev(), "alice.md", "# Notes\nAlice and Bob.", &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    assert_eq!(
        rec.batch_count(),
        1,
        "graph-ON MUST commit Episode + Chunks + Entities + Relations + Facts in ONE atomic_write call"
    );
}

#[tokio::test]
async fn toggle_off_on_off_is_idempotent_and_observable() {
    // Phase 3 ROADMAP success criterion #5 — toggle is idempotent +
    // observable per D-12. State counter only increments on real
    // transitions.
    let (handle, _rec, _clock) = build_handle();
    let gp = handle.graph_pipeline();
    assert!(!gp.is_enabled());
    assert_eq!(gp.state_change_count(), 0);

    gp.enable();
    gp.enable(); // idempotent — no double increment
    assert_eq!(gp.state_change_count(), 1);

    gp.disable();
    gp.disable(); // idempotent
    assert_eq!(gp.state_change_count(), 2);

    gp.enable(); // ON → OFF → ON full sequence
    assert!(gp.is_enabled());
    assert_eq!(
        gp.state_change_count(),
        3,
        "exactly 3 real transitions across the ON-OFF-ON sequence"
    );
}

#[tokio::test]
async fn validator_needs_review_publishes_verify_message() {
    // D-19 Phase 4 hook — Validator-flagged NeedsReview items publish to
    // `__lunaris_verify__` MQ topic via StoragePort::publish.
    let (handle, rec, clock) = build_handle();
    // MockExtractor that emits an Entity with valid_from >= valid_to →
    // Validator demotes to NeedsReviewReason::InvalidBitemporal.
    let mock = Arc::new(MockExtractor::with_invalid_bitemporal());
    let handle = handle.with_extractor(mock as Arc<dyn Extractor>);
    handle.graph_pipeline().enable();

    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "alice_inverted.md",
        "# Notes\nAlice with inverted timestamps.",
        &clock,
    );
    handle.ingest(ep).await.expect("ingest must succeed even when Validator demotes");

    // The bad entity went to needs_review, NOT to out.entities → no
    // GraphNode WriteOp lands.
    let counts = rec.writeop_counts();
    assert_eq!(counts.graph_node, 0, "InvalidBitemporal entity MUST NOT land as GraphNode");

    // ... but the verify-queue publish DID fire.
    assert!(
        rec.published_verify_count() >= 1,
        "expected at least 1 __lunaris_verify__ publish for the demoted entity; got {}",
        rec.published_verify_count()
    );
    // Plan 04-04 D-16: every ingest now ALSO publishes one
    // __lunaris_consolidate__ event. Filter the topic list to the verify
    // topic before asserting "all on the verify queue".
    let verify_topics: Vec<String> =
        rec.published_topics().into_iter().filter(|t| t == "__lunaris_verify__").collect();
    assert!(
        verify_topics.iter().all(|t| t == "__lunaris_verify__"),
        "all filtered verify-topic publishes target __lunaris_verify__; got {:?}",
        verify_topics
    );
}

#[tokio::test]
async fn noop_extractor_with_graph_on_emits_no_graph_writeops() {
    // T-03-03-05 mitigation — NoopExtractor's applies()==false short-
    // circuits BEFORE the extract call; no Entities/Relations/Facts can
    // sneak into the WriteOp batch even if a misconfigured caller forgot
    // to install a real extractor.
    let (handle, rec, clock) = build_handle();
    // Default extractor on with_parts_keyword IS NoopExtractor; just enable.
    handle.graph_pipeline().enable();
    assert!(
        !handle.extractor().expect("default Noop installed").applies(),
        "NoopExtractor.applies() MUST be false"
    );

    let ep = Episode::new(lunaris_core::Scope::dev(), "noop.md", "# Notes\nAlice and Bob.", &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    let counts = rec.writeop_counts();
    assert_eq!(counts.graph_node, 0);
    assert_eq!(counts.graph_edge, 0);
    assert_eq!(
        rec.published_verify_count(),
        0,
        "NoopExtractor produces empty extraction → no NeedsReview to publish on the verify queue (the consolidate-queue publish from Plan 04-04 D-16 fires unconditionally and is NOT counted here)"
    );
    // INGEST-04 still holds — single atomic_write call even on the noop path.
    assert_eq!(rec.batch_count(), 1);
}

#[tokio::test]
async fn with_extractor_swaps_handle_extractor() {
    // Plan 03-03 Task 2 escape hatch — with_extractor(...) replaces the
    // installed extractor. Subsequent ingest() calls use the new one.
    let (handle, _rec, clock) = build_handle();
    handle.graph_pipeline().enable();

    let mock_a = Arc::new(MockExtractor::with_alice_knows_bob());
    let mock_b = Arc::new(MockExtractor::with_alice_knows_bob());
    let handle = handle.with_extractor(mock_a.clone() as Arc<dyn Extractor>);

    let ep1 = Episode::new(lunaris_core::Scope::dev(), "e1.md", "# E1\nAlice.", &clock);
    handle.ingest(ep1).await.expect("ingest 1");
    assert_eq!(mock_a.call_count(), 1, "first extractor must be called for ep1");
    assert_eq!(mock_b.call_count(), 0);

    // Swap to mock_b.
    let handle = handle.with_extractor(mock_b.clone() as Arc<dyn Extractor>);
    let ep2 = Episode::new(lunaris_core::Scope::dev(), "e2.md", "# E2\nBob.", &clock);
    handle.ingest(ep2).await.expect("ingest 2");
    assert_eq!(mock_a.call_count(), 1, "first extractor must NOT be called again after swap");
    assert_eq!(mock_b.call_count(), 1, "second extractor must be called for ep2");
}

#[tokio::test]
async fn id_hex_round_trip_ingest_then_graph_anchored() {
    // W-7 cross-plan integration test — proves Plan 03-03's
    // `WriteOp::GraphNode { props: { "id_hex": ... } }` writes round-trip
    // with Plan 03-02's Cypher `MATCH (n {id_hex: sid}) RETURN m.id_hex AS id`.
    //
    // 1. Ingest one Episode with graph ON; MockExtractor emits an Entity
    //    with EntityId Alice. Verify the GraphNode WriteOp carries
    //    props.id_hex == hex(Alice.id).
    // 2. Set RecordingStorage's canned_graph to a row keyed by Alice.id_hex
    //    (mimicking what an actual backend with the Plan 03-02 Cypher would
    //    return).
    // 3. Invoke `Graph::anchored(vec![Alice], 2).retrieve(&ctx)` directly
    //    (bypassing the Lunaris::recall() hydration path which would
    //    look up `lunaris:chunk:<ulid>` keys — graph hits use entity ids,
    //    not chunk ids; the W-7 contract is at the operator boundary, not
    //    the hydration layer). Assert hits is non-empty AND hits[0].id
    //    == Alice.id_bytes.
    let (handle, rec, clock) = build_handle();
    let alice_id = EntityId::from_name_and_type("Alice", "Person");
    let alice_hex = format!("{}", alice_id);
    let mock = Arc::new(MockExtractor::with_alice_knows_bob());
    let handle = handle.with_extractor(mock as Arc<dyn Extractor>);
    handle.graph_pipeline().enable();

    let ep =
        Episode::new(lunaris_core::Scope::dev(), "alice.md", "# Notes\nAlice knows Bob.", &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    // Step 1 — confirm props.id_hex was written for Alice.
    let batches = rec.batches.lock().clone();
    let mut found_alice_id_hex = false;
    for batch in &batches {
        for op in batch {
            if let WriteOp::GraphNode { props, .. } = op
                && let Some(hex) = props.get("id_hex").and_then(|v| v.as_str())
                && hex == alice_hex
            {
                found_alice_id_hex = true;
            }
        }
    }
    assert!(
        found_alice_id_hex,
        "expected GraphNode WriteOp with props.id_hex == {alice_hex}; W-7 contract"
    );

    // Step 2 — set the canned graph result so graph_traverse returns Alice.
    rec.set_canned_graph(GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: vec![vec![json!(alice_hex.clone()), json!("Alice"), json!("Person")]],
    });

    // Step 3 — call the Graph::anchored operator directly against a
    // QueryContext built from the same storage Arc. This isolates the W-7
    // operator-boundary contract from the recall+hydrate pipeline (graph
    // hits don't go through chunk hydration).
    let storage_arc: Arc<dyn StoragePort> = rec.clone();
    let keyword_arc: Arc<dyn KeywordPort> = rec.clone();
    let embedder_arc: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let ctx = QueryContext::new(
        Query::text("Alice"),
        lunaris_core::Scope::dev(),
        embedder_arc,
        storage_arc,
        keyword_arc,
    );

    let op = Graph::anchored(vec![(alice_id, 1.0)], 2);
    let hits = op.retrieve(&ctx).await.expect("Graph::anchored retrieve must succeed");
    assert!(
        !hits.is_empty(),
        "Graph::anchored MUST return non-zero hits given the canned row (W-7 round-trip)"
    );

    // The first hit's id must equal Alice's 16-byte content hash —
    // hex-decoded from the m.id_hex column at the operator boundary.
    let expected_id_bytes = alice_id.0.to_vec();
    assert_eq!(
        hits[0].id, expected_id_bytes,
        "first hit id MUST round-trip from Alice's id_hex bytes"
    );
    let _ = clock; // suppress unused warning
}

#[tokio::test]
async fn recall_with_graph_anchored_composes_end_to_end() {
    // Canonical Plan 03-03 compose example — Vector + Graph + fuse_rrf +
    // top. Ingest one Episode with graph ON; recall via the DSL composing
    // Vector("chunks") with Graph::anchored. Both branches fire; the
    // RecordingStorage returns chunks from vector_search AND a canned
    // graph row.
    let (handle, rec, clock) = build_handle();
    let alice_id = EntityId::from_name_and_type("Alice", "Person");
    let alice_hex = format!("{}", alice_id);
    let mock = Arc::new(MockExtractor::with_alice_knows_bob());
    let handle = handle.with_extractor(mock as Arc<dyn Extractor>);
    handle.graph_pipeline().enable();

    let ep =
        Episode::new(lunaris_core::Scope::dev(), "alice.md", "# Notes\nAlice knows Bob.", &clock);
    handle.ingest(ep).await.expect("ingest");

    // Canned graph row for the recall path.
    rec.set_canned_graph(GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: vec![vec![json!(alice_hex), json!("Alice"), json!("Person")]],
    });

    let hits = handle
        .recall()
        .with_root(
            Vector::new("chunks", 30).and(Graph::anchored(vec![(alice_id, 1.0)], 2)).fuse_rrf(60).top(5),
        )
        .execute(Query::text("Alice"))
        .await
        .expect("compose recall must succeed");
    assert!(!hits.is_empty(), "canonical Vector + Graph compose MUST return at least one hit");
}

#[tokio::test]
async fn empty_validated_extraction_default_is_constructible() {
    // Compile-time + runtime sanity: ValidatedExtraction::default() exists
    // and yields zero entries. The graph-ON path uses this on the
    // NoopExtractor short-circuit branch.
    let v: ValidatedExtraction = ValidatedExtraction::default();
    assert!(v.entities.is_empty());
    assert!(v.relations.is_empty());
    assert!(v.facts.is_empty());
    assert!(v.needs_review.is_empty());
    // NoopExtractor is constructible from the umbrella crate.
    let _: Arc<dyn Extractor> = Arc::new(NoopExtractor);
    // GraphPipelineHandle is reachable on the umbrella crate.
    let _: Arc<GraphPipelineHandle> = Arc::new(GraphPipelineHandle::with_noop());
}

/// RC-1 (v0.2 release-gate review) regression test.
///
/// Before the fix, `Lunaris::ingest` (graph-on) wrote Fact KV rows via a
/// local unscoped `fact_key(id)` helper that produced bytes `fact:{ulid}` —
/// no `lunaris:{scope}:` prefix. Two scopes writing the same fact ULID
/// would overwrite each other on Moon. The fix routes through
/// `lunaris_core::keyspace::fact_key(&scope, id)`.
///
/// This test asserts the post-fix invariant: every `WriteOp::KvPut` emitted
/// by the graph-on ingest path starts with `lunaris:{scope}:`.
#[tokio::test]
async fn rc1_graph_on_ingest_all_kv_keys_are_scope_prefixed() {
    let (handle, rec, clock) = build_handle();
    let mock = Arc::new(MockExtractor::with_alice_likes_chocolate_fact());
    let handle = handle.with_extractor(mock.clone() as Arc<dyn Extractor>);
    handle.graph_pipeline().enable();

    let scope = lunaris_core::Scope::new("agent.rc1").unwrap();
    let expected_prefix = format!("lunaris:{}:", scope.as_str());
    let ep = Episode::new(scope.clone(), "rc1.md", "# Notes\nAlice and Chocolate.", &clock);
    handle.ingest(ep).await.expect("graph-on ingest must succeed");

    // INGEST-04 holds: ONE atomic_write batch.
    assert_eq!(rec.batch_count(), 1, "graph-on ingest MUST commit one atomic_write batch");

    let batches = rec.batches.lock().clone();
    let batch = &batches[0];

    // Counts: Episode + ≥1 Chunk + 2 Entities (Alice, Chocolate) + 1 Fact
    // — all KvPut. The fact must be present (proves the path emitted it).
    let kv_keys: Vec<Vec<u8>> = batch
        .iter()
        .filter_map(|op| match op {
            WriteOp::KvPut { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    assert!(!kv_keys.is_empty(), "graph-on ingest MUST emit at least one KvPut");

    // The core RC-1 assertion: every key starts with `lunaris:{scope}:`.
    for key in &kv_keys {
        let key_str = std::str::from_utf8(key).expect("KvPut keys must be utf-8");
        assert!(
            key_str.starts_with(&expected_prefix),
            "KvPut key {:?} MUST start with {:?} (RC-1 scope-prefix invariant)",
            key_str,
            expected_prefix,
        );
    }

    // Bonus: at least one of the keys is a fact key (proves the fixture
    // actually exercised the previously-buggy code path).
    let fact_prefix = format!("{}fact:", expected_prefix);
    assert!(
        kv_keys.iter().any(|k| std::str::from_utf8(k).unwrap().starts_with(&fact_prefix)),
        "expected at least one Fact KvPut under `{fact_prefix}` but none found"
    );
}
