//! Ingest pipeline integration tests — INGEST-02..04 invariants.
//!
//! - `single_atomic_write_call`: proves the pipeline calls
//!   [`StoragePort::atomic_write`] EXACTLY ONCE per Episode (INGEST-04).
//! - `embed_fallback_on_batch_error`: proves per-chunk fallback when
//!   `embed_batch` errors on a multi-input call (INGEST-02).
//! - `episode_and_chunks_appear_in_single_batch`: counts the WriteOp shapes
//!   (1 Episode KvPut + 2 ops per chunk: KvPut + VectorUpsert).
//!
//! RFC 0001 Wave 0: `RecordingStorage` updated to accept `&Scope` on all trait methods.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Embedder, Episode, Filter, GraphResult, Hlc, HlcClock, Lsn, LunarisError,
    QueueMsg, Row, Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit,
    WriteOp,
};
use lunaris_ingest::{BpeTokenCounter, TokenCounter, ingest_episode, ingest_episode_with_counter};
use parking_lot::Mutex;

// --------------------------- RecordingStorage ---------------------------

/// In-test `StoragePort` that records every `atomic_write` invocation. Other
/// methods are unsupported (the ingest pipeline only calls `atomic_write`).
#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

impl RecordingStorage {
    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }
    fn first_batch(&self) -> Vec<WriteOp> {
        self.batches.lock().first().cloned().unwrap_or_default()
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
        }
    }
}

// --------------------------- FlakyEmbedder ---------------------------

/// Embedder that errors on the first `embed_batch` invocation if the call has
/// more than one input, then succeeds for every subsequent call. Used to prove
/// the per-chunk fallback path (INGEST-02).
struct FlakyEmbedder {
    inner: StubEmbedder,
    multi_input_call_seen: Mutex<bool>,
    dim: usize,
}

impl FlakyEmbedder {
    fn new(dim: usize) -> Self {
        Self { inner: StubEmbedder::new(dim), multi_input_call_seen: Mutex::new(false), dim }
    }
}

#[async_trait]
impl Embedder for FlakyEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.len() > 1 {
            let mut g = self.multi_input_call_seen.lock();
            if !*g {
                *g = true;
                return Err(LunarisError::Storage(StorageError::Backend(
                    "flaky: simulated batch failure".into(),
                )));
            }
        }
        self.inner.embed_batch(inputs).await
    }
}

// --------------------------- helpers ---------------------------

fn small_episode(clock: &HlcClock) -> Episode {
    let body = "# Section 1\nfoo bar baz qux\n\n## Subsection 1.1\nthis is some prose that the chunker will see and process at runtime.\n\n# Section 2\nmore content with enough words to trigger at least one chunk under the v0 token estimator heuristic of words times 1.3 ceiled.\n";
    Episode::new(Scope::dev(), "smoke.md", body, clock)
}

fn twelve_kb_episode(clock: &HlcClock) -> Episode {
    let body = include_str!("fixtures/12kb_doc.md");
    Episode::new(Scope::dev(), "arch.md", body, clock)
}

// --------------------------- tests ---------------------------

#[tokio::test]
async fn single_atomic_write_call() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = twelve_kb_episode(&clock);

    let lsn = ingest_episode(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "RecordingStorage emits Lsn{{wall_ms=1}}");

    assert_eq!(
        storage.batch_count(),
        1,
        "INGEST-04: pipeline must call atomic_write exactly once per Episode"
    );
}

#[tokio::test]
async fn episode_and_chunks_appear_in_single_batch() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = twelve_kb_episode(&clock);

    ingest_episode(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");

    let batch = storage.first_batch();
    // v0.2.1 keyspace: `lunaris:{scope}:episode:{ulid}` (RFC 0001).
    // twelve_kb_episode binds Scope::dev() ("_dev_"), so the prefix is
    // `lunaris:_dev_:episode:`. Match by infix to stay scope-agnostic if
    // the fixture moves.
    let n_episode_kvput = batch
        .iter()
        .filter(|op| matches!(op, WriteOp::KvPut { key, .. } if key.windows(8).any(|w| w == b":episode")))
        .count();
    let n_chunk_kvput = batch
        .iter()
        .filter(
            |op| matches!(op, WriteOp::KvPut { key, .. } if key.windows(6).any(|w| w == b":chunk")),
        )
        .count();
    let n_vec_upsert = batch
        .iter()
        .filter(|op| matches!(op, WriteOp::VectorUpsert { index, .. } if index == "chunks"))
        .count();
    let n_doctree_kvput = batch
        .iter()
        .filter(
            |op| matches!(op, WriteOp::KvPut { key, .. } if key.windows(8).any(|w| w == b":doctree")),
        )
        .count();
    let n_community_kvput = batch
        .iter()
        .filter(
            |op| matches!(op, WriteOp::KvPut { key, .. } if key.windows(10).any(|w| w == b":community")),
        )
        .count();
    assert_eq!(n_episode_kvput, 1, "exactly one Episode KvPut");
    assert_eq!(n_doctree_kvput, 1, "exactly one DocTree KvPut (STRUCT-02)");
    assert!((4..=8).contains(&n_chunk_kvput), "4–8 chunk KvPuts; got {n_chunk_kvput}");
    assert_eq!(n_chunk_kvput, n_vec_upsert, "every chunk gets a VectorUpsert");
    // Phase 29: Community KvPuts are now added to the batch (one per heading section).
    // Total: 1 DocTree + 1 Episode + 2 per chunk + 1 per community.
    assert_eq!(
        batch.len(),
        2 + 2 * n_chunk_kvput + n_community_kvput,
        "total ops = 1 DocTree + 1 Episode + 2 per chunk + 1 per community; got {}",
        batch.len()
    );
    // Every VectorUpsert carries a 768-d embedding (StubEmbedder dim).
    for op in &batch {
        if let WriteOp::VectorUpsert { embedding, metadata, .. } = op {
            assert_eq!(embedding.len(), 768);
            assert!(metadata.get("episode_id").is_some(), "metadata.episode_id required");
            assert!(metadata.get("heading_path").is_some(), "metadata.heading_path required");
            assert!(metadata.get("offset").is_some(), "metadata.offset required");
        }
    }
}

#[tokio::test]
async fn embed_fallback_on_batch_error() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(FlakyEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = small_episode(&clock);

    ingest_episode(&*storage, &*embedder, &clock, ep)
        .await
        .expect("INGEST-02: per-chunk fallback should rescue batch failure");

    // Even with a flaky embedder, the pipeline still issues exactly ONE atomic_write.
    assert_eq!(storage.batch_count(), 1, "INGEST-04 holds even under embedder fallback");
    let batch = storage.first_batch();
    let n_vec_upsert = batch.iter().filter(|op| matches!(op, WriteOp::VectorUpsert { .. })).count();
    assert!(n_vec_upsert >= 1, "fallback path should still produce embeddings");
    for op in &batch {
        if let WriteOp::VectorUpsert { embedding, .. } = op {
            assert_eq!(embedding.len(), 768, "fallback embeddings must be 768-d");
        }
    }
}

/// Finding 1 guard — production ingest path uses BPE counter, not hardcoded surrogate.
///
/// Ingest the same content via `ingest_episode_with_counter` using the committed
/// test fixture tokenizer (0-merge byte-level BPE). The fixture gives ~1 token
/// per byte (e.g. "hello world" → 11 tokens), which is very different from the
/// surrogate's `words×1.3` heuristic (2 words → 3). We decode the chunk KvPut
/// values from the recorded batch and assert that `chunk.tokens` matches what
/// the BPE counter — NOT the surrogate — would produce.
///
/// This test proves `ingest_episode_with_counter` threads the supplied counter
/// all the way through to the chunk token fields stored in the WriteOp batch.
#[tokio::test]
async fn ingest_episode_with_counter_uses_bpe_not_surrogate() {
    let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/test_tokenizer.json");
    let bpe = std::sync::Arc::new(
        BpeTokenCounter::try_new(&fixture_path).expect("fixture tokenizer must load"),
    );

    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = small_episode(&clock);

    ingest_episode_with_counter(&*storage, &*embedder, &clock, ep, bpe.clone())
        .await
        .expect("ingest with BPE counter must succeed");

    assert_eq!(storage.batch_count(), 1, "INGEST-04: exactly one atomic_write call");

    let batch = storage.first_batch();
    let chunk_kvputs: Vec<&[u8]> = batch
        .iter()
        .filter_map(|op| {
            if let WriteOp::KvPut { key, value } = op {
                if key.windows(6).any(|w| w == b":chunk") {
                    return Some(value.as_slice());
                }
            }
            None
        })
        .collect();

    assert!(!chunk_kvputs.is_empty(), "must have at least one chunk KvPut");

    for raw in chunk_kvputs {
        let chunk: serde_json::Value =
            serde_json::from_slice(raw).expect("chunk KvPut must be valid JSON");
        let stored_tokens = chunk["tokens"].as_u64().expect("chunk.tokens must be a u64") as u32;
        let text = chunk["text"].as_str().expect("chunk.text must be a string");

        // BPE count from the fixture counter.
        let bpe_count = bpe.count(text);
        // Surrogate count (words × 1.3).
        let surrogate_count = (text.split_whitespace().count() as f32 * 1.3).ceil() as u32;

        assert_eq!(
            stored_tokens, bpe_count,
            "chunk.tokens must equal BPE count (text={text:?}, \
             bpe={bpe_count}, surrogate={surrogate_count})"
        );
        // Belt-and-suspenders: BPE and surrogate must differ for this fixture
        // (0-merge byte-level gives ~chars/bytes; surrogate gives words×1.3).
        // If they happen to be equal for a very short chunk, the equality above
        // is still the meaningful assertion.
        if text.len() > 20 {
            assert_ne!(
                bpe_count, surrogate_count,
                "For text longer than 20 chars, BPE (0-merge fixture) and surrogate \
                 must differ (text={text:?})"
            );
        }
    }
}
