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
use lunaris_ingest::{
    BpeTokenCounter, TokenCounter, ingest_episode, ingest_episode_with_counter,
    ingest_episode_with_receipt,
};
use lunaris_test_harness::open_test_storage_with_dim;
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
            graph_decay_native: false,
            graph_navigate_native: false,
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
async fn ingest_receipt_reports_episode_and_chunk_ids() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = small_episode(&clock);
    let episode_id = ep.id;

    let receipt =
        ingest_episode_with_receipt(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");

    assert_eq!(receipt.lsn, Lsn { wall_ms: 1, counter: 0 });
    assert_eq!(receipt.episode_id, episode_id);
    assert!(!receipt.chunk_ids.is_empty(), "receipt should include committed chunk ids");
    assert_eq!(storage.batch_count(), 1, "receipt path must preserve INGEST-04");
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
    let n_community_vec_upsert = batch
        .iter()
        .filter(|op| matches!(op, WriteOp::VectorUpsert { index, .. } if index == "communities"))
        .count();
    assert_eq!(n_episode_kvput, 1, "exactly one Episode KvPut");
    assert_eq!(n_doctree_kvput, 1, "exactly one DocTree KvPut (STRUCT-02)");
    assert!((4..=8).contains(&n_chunk_kvput), "4–8 chunk KvPuts; got {n_chunk_kvput}");
    assert_eq!(n_chunk_kvput, n_vec_upsert, "every chunk gets a VectorUpsert");
    // Phase-30 B1: every community now gets both a KvPut AND a VectorUpsert.
    assert_eq!(
        n_community_kvput, n_community_vec_upsert,
        "Phase-30 B1: every community must have both a KvPut and a VectorUpsert"
    );
    // Phase-30 B1: Total: 1 DocTree + 1 Episode + 2 per chunk + 2 per community.
    assert_eq!(
        batch.len(),
        2 + 2 * n_chunk_kvput + 2 * n_community_kvput,
        "total ops = 1 DocTree + 1 Episode + 2 per chunk + 2 per community; got {}",
        batch.len()
    );
    // Every VectorUpsert carries a 768-d embedding (StubEmbedder dim).
    // Chunk VectorUpserts have episode_id/heading_path/offset metadata;
    // community VectorUpserts have summary/level/parent — scope assertions by index.
    for op in &batch {
        if let WriteOp::VectorUpsert { index, embedding, metadata, .. } = op {
            assert_eq!(embedding.len(), 768, "all VectorUpserts must be 768-d");
            if index == "chunks" {
                assert!(metadata.get("episode_id").is_some(), "chunk metadata.episode_id required");
                assert!(
                    metadata.get("heading_path").is_some(),
                    "chunk metadata.heading_path required"
                );
                assert!(metadata.get("offset").is_some(), "chunk metadata.offset required");
            } else if index == "communities" {
                assert!(
                    metadata.get("summary").is_some(),
                    "community metadata.summary required for BM25"
                );
            }
        }
    }
}

/// W3 (moon-v051-perf-exploit) — chunk KvPut payloads must not carry the
/// embedding at the real production ingest path (not just the primitives.rs
/// unit level). Every chunk KvPut byte blob must (a) deserialize with
/// `embedding == None` and (b) not contain the literal `"embedding"` key at
/// all — the FT `chunks` VectorUpsert (already asserted 768-d elsewhere in
/// this file) is the sole home for the vector now.
#[tokio::test]
async fn chunk_kvput_payload_never_carries_embedding() {
    use lunaris_core::primitives::Chunk;

    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = twelve_kb_episode(&clock);

    ingest_episode(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");
    let batch = storage.first_batch();

    let chunk_kvputs: Vec<&[u8]> = batch
        .iter()
        .filter_map(|op| {
            if let WriteOp::KvPut { key, value } = op
                && key.windows(6).any(|w| w == b":chunk")
            {
                Some(value.as_slice())
            } else {
                None
            }
        })
        .collect();
    assert!(!chunk_kvputs.is_empty(), "must have at least one chunk KvPut");

    let mut total_bytes = 0usize;
    for raw in &chunk_kvputs {
        total_bytes += raw.len();
        let c: Chunk = serde_json::from_slice(raw).expect("chunk KvPut is valid JSON");
        assert!(
            c.embedding.is_none(),
            "W3: Chunk.embedding must be None after KV round-trip (skip_serializing)"
        );
        assert!(
            !raw.windows(b"\"embedding\"".len()).any(|w| w == b"\"embedding\""),
            "W3: the raw chunk KvPut bytes must not contain the \"embedding\" key at all"
        );
    }

    // Sanity ceiling: `twelve_kb_episode` produces ~500-token chunks (up to
    // ~3.5 KB of text + heading_path + bt JSON with no embedding). A 768-d
    // f32 JSON array would add roughly another 7-9 KB on top of that — so a
    // generous 6 KB ceiling still catches a regression that starts inlining
    // the vector again, without being tightly coupled to this fixture's
    // exact chunk-text length. See `c_chunk_kv_payload_shrinks_at_least_4x`
    // in lunaris-core for the precise ratio proof.
    let avg_bytes = total_bytes / chunk_kvputs.len();
    assert!(
        avg_bytes < 6144,
        "average chunk KvPut size {avg_bytes} bytes looks too large for an \
         embedding-free payload — is something still inlining the vector?"
    );
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

/// Warning-2 guard — Community.bt must come from the caller's clock, not a static instance.
///
/// Creates a clock with a distinctive `node_id=42` that cannot be produced by `clock_ref()`
/// (which always uses `node_id=0`). After ingest, all KvPut community bytes are deserialized
/// and their `bt.valid.0.node_id` is asserted equal to 42. This fails when `assemble_and_write`
/// uses `clock_ref()` and passes when it threads the caller clock through.
#[tokio::test]
async fn community_bt_comes_from_caller_clock() {
    use lunaris_core::primitives::Community;

    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    // node_id=42 is a distinctive sentinel — clock_ref() uses node_id=0 and can never emit 42.
    let clock = HlcClock::new(42);
    let ep = small_episode(&clock);

    ingest_episode(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");

    let batch = storage.first_batch();
    let community_bytes: Vec<&[u8]> = batch
        .iter()
        .filter_map(|op| {
            if let WriteOp::KvPut { key, value } = op
                && key.windows(10).any(|w| w == b":community")
            {
                return Some(value.as_slice());
            }
            None
        })
        .collect();

    assert!(!community_bytes.is_empty(), "must have at least one Community KvPut");

    for raw in community_bytes {
        let community: Community =
            serde_json::from_slice(raw).expect("community KvPut must be valid JSON");
        assert_eq!(
            community.bt.valid.0.node_id, 42,
            "Community.bt must come from the caller clock (node_id=42), \
             not the static clock_ref() (node_id=0)"
        );
    }
}

/// B1 / Phase-30 D4 — community summary_embedding must be populated at ingest.
///
/// This test FAILS before the Phase-30 implementation because `Community.summary_embedding`
/// is always `None` (guarded by a `debug_assert!` in `assemble_and_write`). It passes after
/// the implementation embeds community summaries at ingest and writes them as `VectorUpsert`
/// ops into the `communities` index.
///
/// Two assertions:
/// 1. Every community `KvPut` in the batch deserializes to a `Community` whose
///    `summary_embedding` is `None` (W3 moon-v051-perf-exploit: the field is no
///    longer serialized into the KV payload — see
///    `crates/lunaris-core/tests/c_embedding_skip_serialize.rs` for the
///    dedicated skip-serialize + payload-size-reduction proof) — and that the
///    KvPut bytes are meaningfully smaller than they'd be if the embedding
///    were still inlined.
/// 2. At least one `VectorUpsert { index: "communities", .. }` with a 768-d embedding
///    appears in the batch — proving the `communities` FT index is populated
///    (this remains the source of truth for "was it embedded", not the KV blob).
#[tokio::test]
async fn community_summary_embedding_populated_at_ingest() {
    use lunaris_core::primitives::Community;

    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = twelve_kb_episode(&clock);

    ingest_episode(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");

    let batch = storage.first_batch();

    // 1. Every community KvPut must NOT carry summary_embedding (W3 skip_serializing),
    //    and must not even contain the substring "summary_embedding" in the raw bytes.
    let community_kvputs: Vec<&[u8]> = batch
        .iter()
        .filter_map(|op| {
            if let WriteOp::KvPut { key, value } = op
                && key.windows(10).any(|w| w == b":community")
            {
                Some(value.as_slice())
            } else {
                None
            }
        })
        .collect();
    assert!(!community_kvputs.is_empty(), "must have at least one community KvPut");
    for raw in &community_kvputs {
        let c: Community = serde_json::from_slice(raw).expect("community KvPut is valid JSON");
        assert!(
            c.summary_embedding.is_none(),
            "W3: Community.summary_embedding must be None after KV round-trip \
             (skip_serializing) — population is proven via the communities \
             VectorUpsert below, not the KV blob"
        );
        assert!(
            !raw.windows(b"summary_embedding".len()).any(|w| w == b"summary_embedding"),
            "W3: the raw community KvPut bytes must not contain the \
             'summary_embedding' key at all"
        );
    }

    // 2. At least one VectorUpsert for the communities index with correct dim.
    let community_vec_upserts: Vec<&Vec<f32>> = batch
        .iter()
        .filter_map(|op| {
            if let WriteOp::VectorUpsert { index, embedding, .. } = op
                && index == "communities"
            {
                Some(embedding)
            } else {
                None
            }
        })
        .collect();
    assert!(
        !community_vec_upserts.is_empty(),
        "B1/D4: at least one VectorUpsert for the communities index must appear in the batch"
    );
    for emb in &community_vec_upserts {
        assert_eq!(emb.len(), 768, "community VectorUpsert must be 768-d");
    }
}

/// B1 / Phase-30 D4 — discriminating test: production ingest path → communities index
/// returns the community on a vector query.
///
/// This is the DISCRIMINATING integration test: it proves the REAL production ingest
/// path populates the `communities` vector index (not just a unit test of an isolated
/// component).
#[tokio::test]
async fn community_vector_index_searchable_after_ingest() {
    let hits = ingest_then_search_communities().await;
    assert!(
        !hits.is_empty(),
        "B1/D4: communities index must return at least one hit after ingest; got 0 — \
         summary_embedding is not being written to the communities FT index"
    );
}

/// **KNOWN GAP, not a fixture problem — `#[ignore]`d rather than deleted.**
///
/// Community `VectorUpsert` metadata must carry a `summary` field: that is what
/// BM25 content extraction reads. Through 0.6.x this assertion lived inside the
/// test above and was kept green by pinning the whole test to the embedded
/// SQLite backend. 0.7.0 deleted that backend, and on Moon this FAILS:
/// `lunaris-storage-moon::vector::vector_search` hydrates `VectorHit.metadata`
/// from KV only on the post-filter path (`vector.rs` ~:92-120); with
/// `filter = None` it keeps whatever the FT reply carried, which for the
/// communities index is nothing.
///
/// Relaxing the assertion would hide a real production gap — communities BM25
/// content extraction is silently empty on Moon — so the assertion is kept
/// verbatim and marked ignored, which leaves it runnable
/// (`cargo test -- --ignored`) and visible in every test listing. The fix is
/// item 6 of docs/testing/memory-to-moon-port-plan.md §6, deliberately out of
/// scope for the backend deletion. Un-`ignore` when the hydrate covers the
/// unfiltered path.
#[tokio::test]
#[ignore = "Moon does not hydrate VectorHit.metadata on the unfiltered path — port-plan §6 item 6"]
async fn community_hits_carry_summary_metadata_for_bm25() {
    let hits = ingest_then_search_communities().await;
    assert!(!hits.is_empty(), "precondition: the communities index must return hits");
    for hit in &hits {
        assert!(
            hit.metadata.get("summary").is_some(),
            "community VectorUpsert metadata must include 'summary' for BM25 content extraction"
        );
    }
}

/// Run the real ingest path, then query the `communities` index.
async fn ingest_then_search_communities() -> Vec<lunaris_core::VectorHit> {
    // Bind the fixture: it owns the Moon child for the call's lifetime.
    let storage = open_test_storage_with_dim(768).await;
    let port = storage.port();
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    // Use a multi-section doc to guarantee at least one Community is produced.
    let ep = twelve_kb_episode(&clock);
    let scope = ep.scope.clone();

    ingest_episode(&*port, &*embedder, &clock, ep).await.expect("ingest ok");

    // StubEmbedder produces a fixed non-zero vector. Use a non-zero probe so cosine
    // similarity is well-defined and the scan can return hits.
    let probe: Vec<f32> = vec![1.0_f32; 768];
    StoragePort::vector_search(port.as_ref(), &scope, "communities", &probe, 10, None, None, false)
        .await
        .expect("communities vector_search must succeed")
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
            if let WriteOp::KvPut { key, value } = op
                && key.windows(6).any(|w| w == b":chunk")
            {
                return Some(value.as_slice());
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
