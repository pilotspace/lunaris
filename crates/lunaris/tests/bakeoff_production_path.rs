//! T-28-07 production-path integration test.
//!
//! Drives `Lunaris::ingest` (the real production entry point) with an installed
//! [`lunaris_ingest::BakoffConfig`] and proves that:
//!
//! 1. Exactly ONE `atomic_write` call is issued per ingest (INGEST-04).
//! 2. The persisted chunk texts differ from what a structural-only pass at the
//!    default 500-token target would produce (the bakeoff config at target=20
//!    causes RSM to win and produce different boundaries).
//! 3. The total number of `embed_batch` calls is bounded (SINGLE-PASS: winner
//!    embeddings are reused, not re-computed).
//!
//! This is the test that satisfies `wiring_test_name` in StructuredOutput —
//! it proves the **production path** (`Lunaris::ingest`) persists the
//! non-structural winner without re-embedding.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris::Lunaris;
use lunaris_core::{
    CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock, Lsn, LunarisError, QueueMsg, Row,
    Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit, WriteOp,
};
use lunaris_ingest::{
    BakoffConfig, RecursiveSplitMergeGenerator, SelectorWeights, SurrogateTokenCounter,
    chunk_markdown_with_counter,
};
use parking_lot::Mutex;

// ── Recording storage ─────────────────────────────────────────────────────────

/// Captures `atomic_write` batches for post-ingest assertions.
#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

impl RecordingStorage {
    /// Extract all persisted chunk texts from KvPut payloads.
    fn chunk_texts(&self) -> Vec<String> {
        let guard = self.batches.lock();
        let mut texts = Vec::new();
        for batch in guard.iter() {
            for op in batch {
                if let WriteOp::KvPut { value, .. } = op {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(value) {
                        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                }
            }
        }
        texts
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 0 })
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
        Err(StorageError::NotSupported("RecordingStorage::vector_search"))
    }
    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::scan_range"))
    }
    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::read_as_of"))
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
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
        }
    }
}

// ── Counting embedder (wraps StubEmbedder) ───────────────────────────────────

/// Wraps [`StubEmbedder`] and counts `embed_batch` calls so tests can assert
/// the SINGLE-PASS invariant without inspecting internals.
struct CountingEmbedder {
    inner: StubEmbedder,
    call_count: Mutex<usize>,
}

impl CountingEmbedder {
    fn new(dim: usize) -> Self {
        Self { inner: StubEmbedder::new(dim), call_count: Mutex::new(0) }
    }
    fn calls(&self) -> usize {
        *self.call_count.lock()
    }
}

#[async_trait]
impl lunaris_core::Embedder for CountingEmbedder {
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        *self.call_count.lock() += 1;
        self.inner.embed_batch(texts).await
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `BakoffConfig` with RSM as the only non-structural generator and
/// weights that strongly favour semantic coherence.
///
/// `target_tokens=20` is passed to `Lunaris::ingest` via
/// `BakoffConfig::target_tokens` — at this small target, RSM produces different
/// boundaries than structural-at-500 (proven in the lunaris-ingest unit tests).
fn bakeoff_cfg() -> BakoffConfig {
    BakoffConfig {
        weights: SelectorWeights { sc: 0.80, icc: 0.05, dcc: 0.05, bi: 0.05, rc: 0.05 },
        max_candidates: 2,
        generators: vec![Box::new(RecursiveSplitMergeGenerator)],
        // target_tokens=20: forces RSM to produce multiple small chunks rather
        // than the single ~810-token chunk structural-at-500 produces.
        target_tokens: 20,
        overlap_tokens: 5,
    }
}

/// Fixture document: 5 repetitions of a ~25-word paragraph.
///
/// - Structural at target=500 (SurrogateTokenCounter, words×1.3 ≈ 162 tokens/para):
///   merges all 5 paragraphs into a single 810-token chunk → 1 chunk.
/// - RSM at target=20 with SC-dominant weights (0.80):
///   splits at sentence boundaries aiming for ~20-token chunks → multiple chunks.
///
/// The two outputs are GUARANTEED to differ in count (and text), which is what
/// the discriminating assertion checks.
fn fixture_content() -> String {
    let para = "Alice studied the nature of light. She discovered interference patterns. \
                The results surprised the entire physics community. Her mentor praised her. \
                The paper was published.";
    format!("# Document\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n", para, para, para, para, para)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// **THE WIRING TEST**: drives `Lunaris::ingest` end-to-end with an installed
/// `BakoffConfig` and asserts that:
///
/// 1. Exactly ONE `atomic_write` call is issued (INGEST-04).
/// 2. Chunks were persisted (non-empty).
/// 3. The bakeoff produces MORE chunks than the structural-at-500 baseline
///    (structural merges 5 paragraphs → 1 chunk; RSM-at-20 splits them → many).
///
/// This is the production-path proof: `Lunaris::ingest` → `ingest_episode_with_bakeoff`
/// → `assemble_and_write` → single `storage.atomic_write`.
#[tokio::test]
async fn bakeoff_production_path_via_lunaris_ingest() {
    let content = fixture_content();

    // ── Baseline: structural chunks at default 500-token target ──────────────
    // The 5-paragraph fixture at 500-token target collapses to 1 chunk.
    let counter = SurrogateTokenCounter;
    let structural_baseline: Vec<String> =
        chunk_markdown_with_counter(&content, 500, 100, &counter)
            .into_iter()
            .map(|d| d.text)
            .collect();
    // Sanity: structural-at-500 should produce exactly 1 chunk for this fixture.
    assert_eq!(
        structural_baseline.len(),
        1,
        "fixture assumption broken: structural-at-500 should give 1 chunk, got {}",
        structural_baseline.len()
    );

    // ── Production path: Lunaris::ingest with bakeoff config ─────────────────
    let clock = HlcClock::new(0);
    let recording: Arc<RecordingStorage> = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));

    let storage_dyn: Arc<dyn StoragePort> = recording.clone();
    let handle = Lunaris::with_parts(storage_dyn, embedder, clock.clone())
        .with_bakeoff(Arc::new(bakeoff_cfg()));

    let scope = Scope::dev();
    let ep = Episode::new(scope, "test.md", content, &clock);
    let lsn = handle.ingest(ep).await.expect("ingest must succeed");

    // LSN is non-zero (RecordingStorage emits wall_ms=1).
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "expected non-zero LSN, got {lsn:?}");

    // INGEST-04: exactly ONE atomic_write call.
    let batch_count = recording.batches.lock().len();
    assert_eq!(batch_count, 1, "INGEST-04 violated: expected 1 atomic_write, got {batch_count}");

    // Chunks were persisted.
    let persisted = recording.chunk_texts();
    assert!(
        !persisted.is_empty(),
        "no chunk texts were persisted — bakeoff path is not writing chunks"
    );

    // RSM at target=20 produces more chunks than structural-at-500 (which gave 1).
    // This proves the bakeoff config is being applied through Lunaris::ingest.
    assert!(
        persisted.len() > structural_baseline.len(),
        "bakeoff path persisted {} chunk(s), same as or fewer than structural-at-500 ({} chunk(s)); \
         the bakeoff config is not being applied through Lunaris::ingest",
        persisted.len(),
        structural_baseline.len()
    );
}

/// SINGLE-PASS proof via `Lunaris::ingest`: the total `embed_batch` calls
/// after a full bakeoff ingest must be bounded by `1 + N_candidates + 1`.
///
/// Structural (baseline) + RSM (1 extra generator) = 2 candidates.
/// Unit embed (1 call) + 2 candidate chunk embeds + 1 community summary embed = 4 max calls.
/// The +1 community embed is Phase-30 B1: new text (summary), not a re-embed of winner chunks.
/// No additional call is made to re-embed the winner for storage (SINGLE-PASS).
#[tokio::test]
async fn bakeoff_single_pass_via_lunaris_ingest() {
    let clock = HlcClock::new(0);
    let recording: Arc<RecordingStorage> = Arc::new(RecordingStorage::default());
    let counting = Arc::new(CountingEmbedder::new(768));
    let embedder_dyn: Arc<dyn lunaris_core::Embedder> = counting.clone();

    let storage_dyn: Arc<dyn StoragePort> = recording.clone();
    let handle = Lunaris::with_parts(storage_dyn, embedder_dyn, clock.clone())
        .with_bakeoff(Arc::new(bakeoff_cfg()));

    let scope = Scope::dev();
    let ep = Episode::new(scope, "single_pass.md", fixture_content(), &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    // N_candidates = 2 (structural + RSM).
    // Scoring: 1 unit-embed call + 2 candidate chunk-embed calls = 3 total.
    // Phase-30 B1: +1 call for community summary embed (new text, not winner re-embed).
    // SINGLE-PASS: 0 extra calls for the winner after selection; total bound is 4.
    // ≤4 still discriminates against a winner re-embed regression (which would produce ≥5).
    let calls = counting.calls();
    assert!(
        calls <= 4,
        "SINGLE-PASS violated: expected ≤4 embed_batch calls, got {calls}. \
         The winner is being re-embedded after bakeoff selection."
    );
}

/// **GRAPH-ON WIRING TEST**: drives `Lunaris::ingest` with graph-ON enabled
/// AND a `BakoffConfig` installed, proving the graph-ON branch also applies
/// the bakeoff rather than falling through to the structural-only default.
///
/// Asserts:
/// 1. Exactly ONE `atomic_write` call is issued (INGEST-04).
/// 2. Chunks were persisted (non-empty).
/// 3. The bakeoff produces MORE chunks than structural-at-500 baseline
///    (same discriminating fixture as the graph-OFF wiring test).
#[tokio::test]
async fn bakeoff_production_path_via_lunaris_ingest_graph_on() {
    let content = fixture_content();

    // ── Baseline: structural chunks at default 500-token target ──────────────
    let counter = SurrogateTokenCounter;
    let structural_baseline: Vec<String> =
        chunk_markdown_with_counter(&content, 500, 100, &counter)
            .into_iter()
            .map(|d| d.text)
            .collect();
    assert_eq!(
        structural_baseline.len(),
        1,
        "fixture assumption broken: structural-at-500 should give 1 chunk, got {}",
        structural_baseline.len()
    );

    // ── Production path: Lunaris::ingest with graph-ON + bakeoff config ──────
    let clock = HlcClock::new(0);
    let recording: Arc<RecordingStorage> = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));

    let storage_dyn: Arc<dyn StoragePort> = recording.clone();
    let handle = Lunaris::with_parts(storage_dyn, embedder, clock.clone())
        .with_bakeoff(Arc::new(bakeoff_cfg()));

    // Enable graph-ON: with_parts pre-installs NoopExtractor (applies()==false
    // → no LLM extraction, no entity/relation WriteOps, but the chunk path runs).
    handle.graph_pipeline().enable();
    assert!(handle.graph_pipeline().is_enabled(), "graph must be ON for this test");

    let scope = Scope::dev();
    let ep = Episode::new(scope, "graph_on_bakeoff.md", content, &clock);
    let lsn = handle.ingest(ep).await.expect("ingest must succeed with graph-ON + bakeoff");

    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "expected non-zero LSN, got {lsn:?}");

    // INGEST-04: exactly ONE atomic_write call (even with graph-ON).
    let batch_count = recording.batches.lock().len();
    assert_eq!(
        batch_count, 1,
        "INGEST-04 violated (graph-ON): expected 1 atomic_write, got {batch_count}"
    );

    // Chunks were persisted.
    let persisted = recording.chunk_texts();
    assert!(!persisted.is_empty(), "no chunk texts persisted (graph-ON bakeoff path)");

    // RSM at target=20 produces more chunks than structural-at-500.
    assert!(
        persisted.len() > structural_baseline.len(),
        "graph-ON bakeoff path persisted {} chunk(s), same as or fewer than structural-at-500 \
         ({} chunk(s)); bakeoff config not being applied through graph-ON path",
        persisted.len(),
        structural_baseline.len()
    );
}

/// SINGLE-PASS proof for the graph-ON path.
///
/// With graph-ON + NoopExtractor (applies()==false), the chunk/embed steps
/// still run — only the LLM extraction is skipped. The bakeoff's embed calls
/// must therefore be bounded the same way as graph-OFF:
///   1 unit-embed + 2 candidate chunk-embeds + 1 community summary embed = 4 max.
/// The +1 is Phase-30 B1 (new text, not a winner re-embed). No winner re-embed.
#[tokio::test]
async fn bakeoff_single_pass_via_lunaris_ingest_graph_on() {
    let clock = HlcClock::new(0);
    let recording: Arc<RecordingStorage> = Arc::new(RecordingStorage::default());
    let counting = Arc::new(CountingEmbedder::new(768));
    let embedder_dyn: Arc<dyn lunaris_core::Embedder> = counting.clone();

    let storage_dyn: Arc<dyn StoragePort> = recording.clone();
    let handle = Lunaris::with_parts(storage_dyn, embedder_dyn, clock.clone())
        .with_bakeoff(Arc::new(bakeoff_cfg()));

    handle.graph_pipeline().enable();

    let scope = Scope::dev();
    let ep = Episode::new(scope, "graph_on_single_pass.md", fixture_content(), &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    // Same bound as graph-OFF: 1 unit-embed + 2 candidate embeds + 1 community summary = 4.
    // Phase-30 B1 adds the community summary embed (new text, not winner re-embed).
    // A winner re-embed on the graph-ON path would produce ≥5.
    let calls = counting.calls();
    assert!(
        calls <= 4,
        "SINGLE-PASS violated (graph-ON): expected ≤4 embed_batch calls, got {calls}. \
         The winner is being re-embedded after bakeoff selection."
    );
}
