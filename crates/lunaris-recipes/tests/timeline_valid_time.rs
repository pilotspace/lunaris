//! RED — `TimelineReconstruction` cannot ingest an event with a historical
//! valid-time, so `.between()` reconstructs nothing (F21, recipe half).
//!
//! `TimelineReconstruction::ingest` is a pure forwarder to
//! `DocumentCorpus::ingest`, which builds an `Episode` and assigns the
//! caller's map to `episode.metadata` verbatim. Nothing in that path ever
//! sets `t_ref`, so the valid axis stays on the ingest instant and
//! `.between(lo, hi)` — which renders `Filter::ValidTimeRange` into Moon's
//! `@valid_time:[lo hi]` — matches on when we WROTE the events. A corpus of
//! January 2025 events ingested today returns 0 rows for January 2025, which
//! is the recipe's entire headline feature failing. Confirmed live on run
//! 32524873697: `expected exactly 6 events ... got 0: []`.
//!
//! ## The contract decided here
//!
//! `DocumentCorpus::ingest` honours a RESERVED metadata key,
//! `DocumentCorpus::VALID_TIME_KEY` (`"valid_time_unix_ms"`), as the
//! episode's real-world instant.
//!
//! Three alternatives were on the table and rejected:
//!
//! * A new typed `ingest_dated` method — `TimelineReconstruction` is capped
//!   at 6 public fns by its own RCPDOC-04 test, and `DocumentCorpus` is
//!   shared by five recipes, so a signature change ripples through all of
//!   them for the benefit of one.
//! * Changing `ingest`'s tuple to a triple — a breaking change to a public
//!   surface that already ships in two SDKs.
//! * Sniffing any metadata key that looks like a timestamp — silently
//!   reinterpreting caller data as a system-level axis is exactly the class
//!   of behaviour that makes a store untrustworthy.
//!
//! The key is unprefixed (not `event_valid_time_unix_ms`) because
//! `DocumentCorpus` serves papers, docs, repos and timelines alike; "event"
//! is one caller's vocabulary, not the corpus's.
//!
//! The key STAYS in `metadata` after being read, so it round-trips and
//! remains available to `Filter::Eq`. Reading it is additive, not a move.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::Lunaris;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort,
};
use parking_lot::Mutex;

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

fn make_handle(rec: Arc<OpRecordingStorage>, clock: Arc<HlcClock>) -> Arc<Lunaris> {
    Arc::new(Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder) as Arc<dyn Embedder>,
        clock,
    ))
}

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

fn meta(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
}

/// The headline contract: an event declared as having happened in January
/// 2025 is stamped with January 2025 on the valid axis, whenever we ingest it.
#[tokio::test]
async fn a_reserved_metadata_key_sets_the_events_real_world_valid_time() {
    let rec = Arc::new(OpRecordingStorage::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec.clone(), clock);
    let timeline = lunaris_recipes::documentary::TimelineReconstruction::new(
        handle,
        lunaris_core::Scope::dev(),
        "timeline:events/",
    );

    // 2025-01-13T10:00:00Z
    let ms: i64 = 1_736_762_400_000;
    timeline
        .ingest(vec![(
            "The relay outage began at ten in the morning and lasted four hours.".to_string(),
            meta(&[
                ("event_id", serde_json::json!("evt-1")),
                (lunaris_recipes::DocumentCorpus::VALID_TIME_KEY, serde_json::json!(ms)),
            ]),
        )])
        .await
        .expect("ingest must succeed");

    let times = chunk_valid_times(&rec);
    assert!(!times.is_empty(), "ingest must emit chunk vector upserts");
    for (i, got) in times.iter().enumerate() {
        assert_eq!(
            *got, ms,
            "chunk {i}: `.between()` filters on this value. Expected the declared event time \
             ({ms}), got {got} — a value near the current wall clock means the recipe is still \
             dropping the caller's valid-time (F21)."
        );
    }
}

/// The reserved key must survive being read. A caller who set it also wants
/// to filter and display it, and a fix that consumed it would break the
/// `Filter::Eq` path without any test noticing.
#[tokio::test]
async fn the_reserved_key_stays_in_metadata_after_being_honoured() {
    let rec = Arc::new(OpRecordingStorage::default());
    let clock = HlcClock::new(0);
    let handle = make_handle(rec.clone(), clock);
    let corpus = lunaris_recipes::DocumentCorpus::new(
        handle,
        lunaris_core::Scope::dev(),
        "timeline:events/",
    );

    let ms: i64 = 1_736_762_400_000;
    corpus
        .ingest(vec![(
            "A short note about the outage.".to_string(),
            meta(&[(lunaris_recipes::DocumentCorpus::VALID_TIME_KEY, serde_json::json!(ms))]),
        )])
        .await
        .expect("ingest must succeed");

    let episodes: Vec<Episode> = rec
        .rows
        .lock()
        .values()
        .filter_map(|v| serde_json::from_slice::<Episode>(v).ok())
        .filter(|e: &Episode| e.source.starts_with("timeline:events/"))
        .collect();
    assert_eq!(episodes.len(), 1, "exactly one episode row must be persisted");
    assert_eq!(
        episodes[0].metadata.get(lunaris_recipes::DocumentCorpus::VALID_TIME_KEY),
        Some(&serde_json::json!(ms)),
        "reading the reserved key must be additive — it stays available to Filter::Eq"
    );
}

/// The vacuity floor, and the fail-safe posture. A corpus entry with no
/// reserved key, or with a value that is not a whole number of milliseconds,
/// falls back to the ingest instant rather than to zero, to a panic, or to a
/// silently mangled date.
#[tokio::test]
async fn a_missing_or_malformed_key_falls_back_to_the_ingest_instant() {
    for bad in [
        None,
        Some(serde_json::json!("2025-01-13")),
        Some(serde_json::json!(1.5)),
        Some(serde_json::json!(null)),
    ] {
        let rec = Arc::new(OpRecordingStorage::default());
        let clock = HlcClock::new(0);
        let handle = make_handle(rec.clone(), clock);
        let corpus = lunaris_recipes::DocumentCorpus::new(
            handle,
            lunaris_core::Scope::dev(),
            "timeline:events/",
        );

        let m = match &bad {
            None => meta(&[]),
            Some(v) => meta(&[(lunaris_recipes::DocumentCorpus::VALID_TIME_KEY, v.clone())]),
        };
        let before = chrono::Utc::now().timestamp_millis();
        corpus.ingest(vec![("A short note.".to_string(), m)]).await.expect("ingest must succeed");
        let after = chrono::Utc::now().timestamp_millis();

        let times = chunk_valid_times(&rec);
        assert!(!times.is_empty(), "ingest must emit chunk vector upserts for {bad:?}");
        for got in times {
            assert!(
                got >= before && got <= after,
                "with a {bad:?} valid-time key the axis must fall back to the ingest instant; \
                 expected within [{before}, {after}], got {got}"
            );
        }
    }
}
