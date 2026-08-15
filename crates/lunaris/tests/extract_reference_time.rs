//! RED — session-date grounding (Mechanism B of the 2026-07-29 N=125 A/B
//! diagnosis): the extraction prompt hallucinates `valid_from` for 78% of
//! items (3,359/4,882 stamped 2025 + 443 stamped 2026 vs 2022-2023
//! haystacks) because the prompt says "else today" and NO reference time
//! ever reaches the extractor — `ChunkInput` carries only chunk_id/text/
//! heading_path even though the episode's real-world date (`Episode::t_ref`)
//! exists at ingest time.
//!
//! Contract pinned here (the production-path half; prompt rendering is
//! pinned in lunaris-extract's own tests — feedback_built_not_wired):
//!
//! 1. `ingest` (graph-ON) threads `Episode::t_ref` to the extractor as
//!    `ChunkInput::reference_time_iso` (date-only `YYYY-MM-DD`, LME's
//!    session granularity) on EVERY chunk input — not just the first chunk,
//!    which is the only one that happens to contain the harness's
//!    `[Session date: ...]` text marker.
//! 2. No `t_ref` → `reference_time_iso == None` (prompt falls back to the
//!    null-over-guess policy, never "today").
//! 3. `WorkingMemory::write_dated` (the seam the LME harness ingests
//!    through via `CodingSessionMemory`) stamps `t_ref` on the Episode it
//!    persists.

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

/// A dated episode's real-world date must reach the extractor on EVERY
/// chunk input as date-only ISO.
#[tokio::test]
async fn dated_episode_threads_reference_time_to_every_chunk_input() {
    let rec = Arc::new(OpRecordingStorage::default());
    let extractor = Arc::new(CapturingExtractor::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec, extractor.clone(), clock.clone());

    // Long multi-paragraph content so the chunker emits >1 chunk — the
    // discriminating shape: only chunk 0 could ever contain an in-text date
    // marker, so a text-scraping implementation fails this test.
    let para = "The launch retrospective covered the relay outage in depth. \
                Multiple teams contributed root-cause notes and follow-ups.\n\n";
    let content = para.repeat(40);
    let mut ep = Episode::new(lunaris_core::Scope::dev(), "dated.md", content, &clock);
    ep.t_ref = Some(chrono::Utc.with_ymd_and_hms(2023, 5, 30, 23, 40, 0).unwrap());
    handle.ingest(ep).await.expect("graph-ON ingest must succeed");

    let seen = extractor.seen.lock();
    assert!(!seen.is_empty(), "extractor must have been invoked");
    let inputs: Vec<&ChunkInput> = seen.iter().flatten().collect();
    assert!(!inputs.is_empty());
    for (i, c) in inputs.iter().enumerate() {
        assert_eq!(
            c.reference_time_iso.as_deref(),
            Some("2023-05-30"),
            "chunk input {i} must carry the episode's date-only reference time"
        );
    }
}

/// No t_ref → None (the prompt's null-over-guess fallback, never "today").
#[tokio::test]
async fn undated_episode_leaves_reference_time_none() {
    let rec = Arc::new(OpRecordingStorage::default());
    let extractor = Arc::new(CapturingExtractor::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec, extractor.clone(), clock.clone());

    let ep = Episode::new(lunaris_core::Scope::dev(), "undated.md", "Some short note.", &clock);
    assert!(ep.t_ref.is_none(), "Episode::new must not invent a t_ref");
    handle.ingest(ep).await.expect("ingest must succeed");

    let seen = extractor.seen.lock();
    let inputs: Vec<&ChunkInput> = seen.iter().flatten().collect();
    assert!(!inputs.is_empty());
    for c in inputs {
        assert_eq!(c.reference_time_iso, None);
    }
}

/// The LME harness ingests through the scratchpad seam — `write_dated` must
/// stamp `t_ref` on the persisted Episode.
#[tokio::test]
async fn working_memory_write_dated_stamps_t_ref() {
    let rec = Arc::new(OpRecordingStorage::default());
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec.clone() as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder) as Arc<dyn Embedder>,
        clock,
    );
    let wm =
        lunaris::WorkingMemory::new(Arc::new(handle), lunaris_core::Scope::dev(), "helios:fs/t/");

    let t = chrono::Utc.with_ymd_and_hms(2023, 6, 1, 9, 0, 0).unwrap();
    wm.write_dated("s1.md", serde_json::Value::String("doc body".into()), t)
        .await
        .expect("write_dated must succeed");

    let episodes: Vec<Episode> = rec
        .rows
        .lock()
        .values()
        .filter_map(|v| serde_json::from_slice::<Episode>(v).ok())
        .filter(|e| e.source.contains("s1.md"))
        .collect();
    assert_eq!(episodes.len(), 1, "exactly one persisted Episode for the write");
    assert_eq!(episodes[0].t_ref, Some(t), "write_dated must stamp t_ref on the Episode");
}
