//! RED — the bi-temporal VALID axis is not caller-settable anywhere, so
//! `Filter::ValidTimeRange` silently filters on INGEST time (F21).
//!
//! Lunaris advertises two independent time axes: `sys` ("when we recorded
//! it") and `valid` ("when it was true in the world"). Only the first one
//! is real. Every primitive constructor stamps `bt: BiTemporal::now(clock)`
//! — both axes get the same ingest instant — and no production path ever
//! calls `BiTemporal::at`, which exists precisely to set them apart. So
//! `chunk.bt.valid.0.wall_ms`, the value `ingest.rs` writes into the Moon
//! chunks index as the `valid_time` NUMERIC field, is the ingest timestamp
//! for every caller of every API.
//!
//! The consequence is that `Filter::ValidTimeRange(lo, hi)` — rendered as
//! `@valid_time:[lo hi]` — answers "what did we WRITE in this window", not
//! "what was TRUE in this window". A corpus of 2025-01 events ingested today
//! matches nothing in January 2025, which is how F21 first surfaced:
//! `TimelineReconstruction::between()` returning 0 rows on run 32524873697.
//!
//! `Episode::t_ref` is the caller's existing channel for "the real-world
//! date of this content". `EpisodeBuilder::t_ref` sets it, `lunaris-hook`
//! populates it from the real event timestamp, and `WorkingMemory::
//! write_dated` stamps it. It reaches exactly one consumer — the extraction
//! prompt's `reference_time_iso` — and is dropped everywhere else. These
//! tests pin the missing half: `t_ref` must also set the valid axis of the
//! episode and of every chunk derived from it.
//!
//! ## Why these tests are shaped this way
//!
//! They assert on the WRITE OPS the production `Lunaris::ingest` path emits,
//! not on rows read back from a backend. A read-back test would pass the
//! moment a backend stopped filtering at all, and would need a live Moon to
//! say anything; this asserts the exact number Lunaris hands the index.
//!
//! The undated case is the vacuity floor. Without it, "valid_time_ms equals
//! the historical date" could be satisfied by an implementation that
//! backdates everything, or by one that writes no `valid_time_ms` at all.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::TimeZone;
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
use lunaris_extract::{ChunkInput, RawExtraction, RawExtractionBatch};
use parking_lot::Mutex;
use ulid::Ulid;

const DIM: usize = 768;

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|_| vec![0.1f32; DIM]).collect())
    }
}

/// Storage that records every atomic_write batch (KvPut rows readable back).
#[derive(Default)]
struct OpRecordingStorage {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
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

/// Extractor that records every ChunkInput batch it is handed.
#[derive(Default)]
struct CapturingExtractor {
    seen: Mutex<Vec<Vec<ChunkInput>>>,
}

#[async_trait]
impl Extractor for CapturingExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        self.seen.lock().push(chunks.to_vec());
        Ok(RawExtractionBatch {
            by_chunk: chunks
                .iter()
                .map(|c| RawExtraction {
                    source_chunk_id: c.chunk_id,
                    entities: vec![],
                    relations: vec![],
                    facts: vec![],
                })
                .collect(),
        })
    }
    fn applies(&self) -> bool {
        true
    }
}

fn make_handle(
    rec: Arc<OpRecordingStorage>,
    extractor: Arc<CapturingExtractor>,
    clock: Arc<HlcClock>,
) -> Lunaris {
    let handle = Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder) as Arc<dyn Embedder>,
        clock,
    );
    handle.graph_pipeline().enable();
    handle.graph_pipeline().set_extractor(extractor);
    handle
}

/// Long enough that the chunker emits more than one chunk — a fix that only
/// backdates chunk 0 fails here.
fn long_content() -> String {
    let para = "The relay outage postmortem ran long. Every team filed \
                root-cause notes, and the follow-ups were tracked to close.\n\n";
    para.repeat(40)
}

/// Pull every `valid_time_ms` the ingest handed to the chunks vector index.
fn chunk_valid_times(rec: &OpRecordingStorage) -> Vec<i64> {
    rec.batches
        .lock()
        .iter()
        .flatten()
        .filter_map(|op| match op {
            WriteOp::VectorUpsert { index, metadata, .. } if index == "chunks" => {
                metadata.get("valid_time_ms").and_then(|v| v.as_i64())
            }
            _ => None,
        })
        .collect()
}

/// The headline contract: a dated episode's chunks carry the REAL-WORLD
/// instant on the valid axis, not the moment we happened to ingest them.
#[tokio::test]
async fn a_dated_episode_stamps_its_chunks_with_the_real_world_valid_time() {
    let rec = Arc::new(OpRecordingStorage::default());
    let extractor = Arc::new(CapturingExtractor::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec.clone(), extractor, clock.clone());

    let t_ref = chrono::Utc.with_ymd_and_hms(2025, 1, 13, 10, 0, 0).unwrap();
    let expected = t_ref.timestamp_millis();

    let mut ep = Episode::new(lunaris_core::Scope::dev(), "dated.md", long_content(), &clock);
    ep.t_ref = Some(t_ref);
    handle.ingest(ep).await.expect("ingest must succeed");

    let times = chunk_valid_times(&rec);
    assert!(times.len() > 1, "the fixture must produce >1 chunk, got {}", times.len());
    for (i, got) in times.iter().enumerate() {
        assert_eq!(
            *got, expected,
            "chunk {i}: `valid_time_ms` is the axis `Filter::ValidTimeRange` filters on. \
             Expected the episode's real-world t_ref ({expected}), got {got}. A value near \
             the current wall clock means the valid axis is still ingest time (F21)."
        );
    }
}

/// The vacuity floor. Without a `t_ref` there is nothing better to say than
/// "now", and that must stay true — otherwise the test above could be
/// satisfied by an implementation that backdates indiscriminately.
#[tokio::test]
async fn an_undated_episode_still_stamps_the_ingest_instant() {
    let rec = Arc::new(OpRecordingStorage::default());
    let extractor = Arc::new(CapturingExtractor::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec.clone(), extractor, clock.clone());

    let before = chrono::Utc::now().timestamp_millis();
    let ep = Episode::new(lunaris_core::Scope::dev(), "undated.md", long_content(), &clock);
    assert!(ep.t_ref.is_none(), "Episode::new must not invent a t_ref");
    handle.ingest(ep).await.expect("ingest must succeed");
    let after = chrono::Utc::now().timestamp_millis();

    let times = chunk_valid_times(&rec);
    assert!(!times.is_empty(), "ingest must emit chunk vector upserts");
    for (i, got) in times.iter().enumerate() {
        assert!(
            *got >= before && *got <= after,
            "chunk {i}: with no t_ref the valid axis must fall back to the ingest instant; \
             expected within [{before}, {after}], got {got}"
        );
    }
}

/// The two axes must actually come apart. This is the property that makes
/// the store bi-temporal rather than mono-temporal with a spare field: a
/// backdated `valid` must not drag `sys` back with it, or an `as_of` system
/// query would claim we knew things before we recorded them.
#[tokio::test]
async fn backdating_the_valid_axis_leaves_the_system_axis_at_ingest_time() {
    let rec = Arc::new(OpRecordingStorage::default());
    let extractor = Arc::new(CapturingExtractor::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec.clone(), extractor, clock.clone());

    let t_ref = chrono::Utc.with_ymd_and_hms(2025, 1, 13, 10, 0, 0).unwrap();
    let before = chrono::Utc::now().timestamp_millis() as u64;
    let mut ep = Episode::new(lunaris_core::Scope::dev(), "dated.md", long_content(), &clock);
    ep.t_ref = Some(t_ref);
    handle.ingest(ep).await.expect("ingest must succeed");
    let after = chrono::Utc::now().timestamp_millis() as u64;

    let chunks: Vec<lunaris_core::Chunk> = rec
        .rows
        .lock()
        .values()
        .filter_map(|v| serde_json::from_slice::<lunaris_core::Chunk>(v).ok())
        .collect();
    assert!(!chunks.is_empty(), "ingest must persist chunk rows");

    for c in &chunks {
        assert_eq!(
            c.bt.valid.0.wall_ms,
            t_ref.timestamp_millis() as u64,
            "the persisted chunk's VALID axis must carry the real-world instant"
        );
        assert!(
            c.bt.sys.0.wall_ms >= before && c.bt.sys.0.wall_ms <= after,
            "the SYSTEM axis must stay at ingest time — backdating `valid` must not move \
             `sys`, or an as_of query would claim we knew this before we recorded it. \
             Expected within [{before}, {after}], got {}",
            c.bt.sys.0.wall_ms
        );
    }
}

/// A dated episode's own row must agree with its chunks. They are two
/// separate stamping sites, so a fix applied to only one of them is a real
/// and likely failure mode.
#[tokio::test]
async fn the_episode_row_carries_the_same_valid_time_as_its_chunks() {
    let rec = Arc::new(OpRecordingStorage::default());
    let extractor = Arc::new(CapturingExtractor::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec.clone(), extractor, clock.clone());

    let t_ref = chrono::Utc.with_ymd_and_hms(2025, 1, 13, 10, 0, 0).unwrap();
    let mut ep = Episode::new(lunaris_core::Scope::dev(), "dated.md", long_content(), &clock);
    ep.t_ref = Some(t_ref);
    handle.ingest(ep).await.expect("ingest must succeed");

    let episodes: Vec<Episode> = rec
        .rows
        .lock()
        .values()
        .filter_map(|v| serde_json::from_slice::<Episode>(v).ok())
        .filter(|e: &Episode| e.source == "dated.md")
        .collect();
    assert_eq!(episodes.len(), 1, "exactly one episode row must be persisted");
    assert_eq!(
        episodes[0].bt.valid.0.wall_ms,
        t_ref.timestamp_millis() as u64,
        "the episode row's valid axis must be backdated too — chunks and episode are \
         stamped at separate sites, and a half-applied fix leaves them disagreeing"
    );
}
