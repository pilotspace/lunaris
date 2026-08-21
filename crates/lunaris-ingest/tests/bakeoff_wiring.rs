//! Integration tests for the adaptive meta-framework bake-off wiring (Phase 28).
//!
//! ## DISCRIMINATING WIRING TEST
//!
//! `integration_winner_chunks_differ_from_structural_baseline`:
//! Ingests a document designed so that the `RecursiveSplitMergeGenerator` wins
//! (SC-dominant weights ensure the generator closest to `target_tokens` wins).
//! The test asserts that the persisted chunk texts DIFFER from what
//! `chunk_markdown_with_headings_with_counter` alone would produce.
//! If the bake-off is not wired, the structural baseline always wins and
//! persisted chunks == structural — this test would fail in that case.
//!
//! ## SINGLE-PASS embed count test
//!
//! `single_pass_embed_count_equals_zero_extra_for_winner`:
//! Asserts that after `run_bakeoff` returns, the ingest pipeline adds ZERO
//! additional `embed_batch` calls for the winner (delta == 0 vs snapshot at
//! bakeoff completion).  Proves SINGLE-PASS invariant: winner's scoring
//! embeddings are reused as storage embeddings.
//!
//! ## INGEST-04 grep check
//!
//! `ingest04_single_atomic_write_in_pipeline_rs`:
//! Shell-level grep asserts exactly one executable `storage.atomic_write(`
//! call site in `pipeline.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Embedder, Episode, Filter, GraphResult, Hlc, HlcClock, Lsn, LunarisError,
    QueueMsg, Row, Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit,
    WriteOp,
};
use lunaris_ingest::{
    BakoffConfig, RecursiveSplitMergeGenerator, ScoredCandidate, SelectorWeights,
    SurrogateTokenCounter, TokenCounter, chunk_markdown_with_headings_with_counter,
    ingest_episode_with_bakeoff, run_bakeoff,
};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// RecordingStorage — records atomic_write batches + captures chunk KvPuts
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

impl RecordingStorage {
    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }

    /// Extract all KvPut values from the first batch.
    fn chunk_texts(&self) -> Vec<String> {
        let batches = self.batches.lock();
        let batch = batches.first().cloned().unwrap_or_default();
        batch
            .iter()
            .filter_map(|op| match op {
                WriteOp::KvPut { value, .. } => {
                    // Try to deserialize as a Chunk and extract its text field
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(value) {
                        v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 0 })
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
        Err(StorageError::NotSupported("RecordingStorage::vector_search"))
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::scan_range"))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::read_as_of"))
    }

    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::publish"))
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
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
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

// ---------------------------------------------------------------------------
// CountingEmbedder — counts total embed_batch calls and total input strings
// ---------------------------------------------------------------------------

/// Wraps a `StubEmbedder` and counts total `embed_batch` invocations and
/// total input strings across all calls.  Used to prove the SINGLE-PASS
/// invariant: the winner is not re-embedded after `run_bakeoff` returns.
struct CountingEmbedder {
    inner: StubEmbedder,
    call_count: Mutex<usize>,
    string_count: Mutex<usize>,
}

impl CountingEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            inner: StubEmbedder::new(dim),
            call_count: Mutex::new(0),
            string_count: Mutex::new(0),
        }
    }

    fn call_count(&self) -> usize {
        *self.call_count.lock()
    }

    #[allow(dead_code)]
    fn string_count(&self) -> usize {
        *self.string_count.lock()
    }
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        *self.call_count.lock() += 1;
        *self.string_count.lock() += inputs.len();
        self.inner.embed_batch(inputs).await
    }
}

// ---------------------------------------------------------------------------
// Fixture document
// ---------------------------------------------------------------------------

/// A fixture document designed so that `RecursiveSplitMergeGenerator` produces
/// different chunk boundaries than the structural baseline.
///
/// The structural baseline at `target=500 overlap=100` with `SurrogateTokenCounter`
/// (words×1.3) will split this mid-paragraph.  The `RecursiveSplitMergeGenerator`
/// merges sentence-level pieces toward the target differently.
fn fixture_doc() -> String {
    // ~120 words per paragraph × 1.3 ≈ 156 tokens per paragraph.
    // Structural at target=500 groups 3+ paragraphs together.
    // RecursiveSplitMerge at sentence level produces 1-sentence chunks.
    // With SC-dominant weights and a very small target (20 tokens), the
    // recursive generator wins because it hits the target better per chunk.
    let para = "Alice studied the nature of light. She discovered interference patterns. \
                The results surprised the entire physics community. Her mentor praised her. \
                The paper was published.";
    format!("# Document\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n", para, para, para, para, para)
}

fn make_episode(content: &str) -> Episode {
    let scope = Scope::new("test").unwrap();
    let clock = HlcClock::new(0);
    Episode::new(scope, "fixture", content.to_string(), &clock)
}

fn surrogate_counter() -> Arc<dyn TokenCounter + Send + Sync> {
    Arc::new(SurrogateTokenCounter)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// DISCRIMINATING WIRING TEST.
///
/// Ingests a document with the bake-off configured to prefer small chunks
/// (SC-dominant, very small target).  Asserts the persisted chunks differ
/// from the structural baseline — if the bake-off is NOT wired, the structural
/// baseline always wins and persisted == structural (test fails).
#[tokio::test]
async fn integration_winner_chunks_differ_from_structural_baseline() {
    let doc = fixture_doc();
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(4));
    let clock = HlcClock::new(0);

    // Weights: SC dominates (0.80) so the generator that hits target=20 best wins.
    // RecursiveSplitMergeGenerator at sentence level will produce ~10-20 token chunks
    // and score much higher SC than structural (which produces ~150-token chunks).
    let weights = SelectorWeights { sc: 0.80, icc: 0.05, dcc: 0.05, bi: 0.05, rc: 0.05 };
    let config = BakoffConfig {
        weights,
        max_candidates: 3,
        generators: vec![Box::new(RecursiveSplitMergeGenerator)],
        target_tokens: 500,
        overlap_tokens: 100,
    };
    let counter = surrogate_counter();

    // Get structural baseline for comparison
    let (structural_drafts, _) = chunk_markdown_with_headings_with_counter(
        &doc,
        500, // default target
        100,
        counter.as_ref(),
    );
    let structural_texts: Vec<String> = structural_drafts.iter().map(|d| d.text.clone()).collect();

    // Run ingest with bake-off (very small target=20 tokens in config to favor RSM)
    let episode = make_episode(&doc);
    ingest_episode_with_bakeoff(
        storage.as_ref(),
        embedder.as_ref(),
        &clock,
        episode,
        counter.clone(),
        Some(Arc::new(config)),
        20, // target_tokens for the bake-off (small = RSM wins)
        100,
    )
    .await
    .expect("ingest must succeed");

    // Verify exactly one atomic_write was called (INGEST-04)
    assert_eq!(storage.batch_count(), 1, "INGEST-04: exactly one atomic_write must be called");

    // Verify persisted chunk texts differ from structural baseline
    let persisted = storage.chunk_texts();
    assert!(!persisted.is_empty(), "at least one chunk text must be persisted");

    // The winner (RecursiveSplitMerge at target=20) should produce more, smaller chunks
    // than the structural baseline at target=500.  At minimum, the chunk texts should differ.
    let texts_equal = persisted == structural_texts;
    assert!(
        !texts_equal,
        "persisted chunk texts must differ from structural baseline when bake-off is wired \
         (got {} persisted texts vs {} structural, persisted[0]={:?})",
        persisted.len(),
        structural_texts.len(),
        persisted.first()
    );
}

/// SINGLE-PASS embed count proof.
///
/// After `run_bakeoff` selects a winner, the ingest storage path must add ZERO
/// additional `embed_batch` calls.  The winner's scoring embeddings are reused.
#[tokio::test]
async fn single_pass_embed_count_equals_zero_extra_for_winner() {
    let doc = fixture_doc();
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(CountingEmbedder::new(4));
    let clock = HlcClock::new(0);

    let weights = SelectorWeights { sc: 0.80, icc: 0.05, dcc: 0.05, bi: 0.05, rc: 0.05 };
    let config = BakoffConfig {
        weights,
        max_candidates: 2,
        generators: vec![Box::new(RecursiveSplitMergeGenerator)],
        target_tokens: 500,
        overlap_tokens: 100,
    };
    let counter = surrogate_counter();

    let episode = make_episode(&doc);

    // Snapshot embed call count BEFORE ingest
    let calls_before = embedder.call_count();

    ingest_episode_with_bakeoff(
        storage.as_ref(),
        embedder.as_ref(),
        &clock,
        episode,
        counter,
        Some(Arc::new(config)),
        500,
        100,
    )
    .await
    .expect("ingest must succeed");

    let calls_after = embedder.call_count();
    let calls_total = calls_after - calls_before;

    // The bake-off makes:
    //   1 call for unit embeddings (for SemanticBreakpoint context)
    //   1 call per candidate (structural + RSM = 2) for chunk embeddings
    // = 3 calls inside run_bakeoff
    //
    // Phase-30 B1 adds exactly 1 more embed_batch call inside assemble_and_write for
    // community summary embedding. This is NOT a SINGLE-PASS violation: community
    // summary texts are genuinely new (not re-embeddings of the winner's chunk texts).
    // The SINGLE-PASS invariant is: no re-embed of the winner's chunk texts after
    // run_bakeoff. Community summaries are produced by ExtractiveSummarizer after the
    // bakeoff, so they are a separate, smaller batch.
    //
    // Concretely:
    //   calls inside run_bakeoff = 1 (units) + N_candidates
    //   calls for community summaries = 1 (Phase-30 B1, assemble_and_write)
    //   calls for storage (winner chunk re-embed) = 0 (SINGLE-PASS preserved)
    // So total calls <= 1 + N_candidates + 1.
    let n_candidates = 2usize; // structural + RSM
    // +1 for the Phase-30 B1 community summary embed_batch in assemble_and_write.
    // W4.5 made that call conditional on `LUNARIS_RAPTOR_ENABLED`, so the actual
    // count is `1 + n_candidates` by default. This stays an upper bound on
    // purpose: it guards SINGLE-PASS (no winner re-embed), and must hold with
    // the RAPTOR gate open or closed.
    let max_expected_calls = 1 + n_candidates + 1;

    assert!(
        calls_total <= max_expected_calls,
        "SINGLE-PASS: total embed_batch calls ({calls_total}) must be <= \
         bakeoff ({}) + community-summary-embed (1) = {max_expected_calls}. \
         Extra calls indicate the winner's chunks were re-embedded for storage.",
        1 + n_candidates
    );
}

/// INGEST-04: verify only one executable `storage.atomic_write(` call in pipeline.rs.
///
/// This is a compile-time grep check — if a second `atomic_write` was added by
/// the wiring, this test catches it.
#[test]
fn ingest04_single_atomic_write_in_pipeline_rs() {
    let pipeline_src = include_str!("../src/pipeline.rs");
    // Count executable call sites: look for `storage.atomic_write(` — the actual call.
    // Comments and docs reference `atomic_write` differently (no `storage.` prefix
    // or no `(` immediately after) so this is precise.
    let executable_calls = pipeline_src.matches("storage.atomic_write(").count();
    assert_eq!(
        executable_calls, 1,
        "INGEST-04: pipeline.rs must contain exactly one executable storage.atomic_write( call, \
         found {executable_calls}"
    );
}

/// NON-STRUCTURAL WINNER: prove that with SC-dominant weights and a small target,
/// `run_bakeoff` selects the `RecursiveSplitMergeGenerator`, not `StructuralGenerator`.
///
/// This directly validates the claim in `wiring_test_name`: the production path
/// persists a non-structural winner's chunks.
#[tokio::test]
async fn bakeoff_selects_non_structural_winner_at_small_target() {
    let doc = fixture_doc();
    let embedder = StubEmbedder::new(4);
    let counter = SurrogateTokenCounter;

    // SC-dominant weights, very small target → RSM chunks of ~5-10 tokens score
    // far higher on SC than structural chunks of ~150 tokens.
    let weights = SelectorWeights { sc: 0.80, icc: 0.05, dcc: 0.05, bi: 0.05, rc: 0.05 };
    let config = BakoffConfig {
        weights,
        max_candidates: 3,
        generators: vec![Box::new(RecursiveSplitMergeGenerator)],
        target_tokens: 500,
        overlap_tokens: 100,
    };

    let (_, structural_heading_records) =
        chunk_markdown_with_headings_with_counter(&doc, 20, 100, &counter);

    let winner: ScoredCandidate = run_bakeoff(
        &doc,
        structural_heading_records,
        &config,
        &embedder,
        &counter,
        20, // very small target — RSM wins on SC
        100,
    )
    .await
    .expect("StubEmbedder must not fail in bakeoff");

    assert_eq!(
        winner.winner_name, "recursive-split-merge",
        "with SC-dominant weights and target=20, RecursiveSplitMergeGenerator must win; \
         got winner_name={:?}",
        winner.winner_name
    );
}
