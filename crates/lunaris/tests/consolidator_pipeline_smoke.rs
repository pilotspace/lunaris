//! Plan 04-04 — consolidator_pipeline_smoke tests.
//!
//! Mirrors graph_pipeline_smoke.rs structure with the in-memory
//! `RecordingStorageWithKeyword` fixture. Validates:
//!
//! - Default-OFF posture (consolidator pipeline is disabled by default).
//! - D-16 publish-after-commit: every Lunaris::ingest publishes one
//!   `__lunaris_consolidate__` envelope (CONSOL-05).
//! - W-2 MockConsolidator skeleton — the Consolidator trait is fully
//!   implementable from the umbrella `lunaris::Consolidator` re-export.
//! - D-12 idempotent toggle observability.
//! - with_consolidator swap preserves toggle + counter (D-12).
//! - NoopConsolidator + pipeline ON emits no audit messages.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::{
    Consolidator, ConsolidatorPipelineHandle, Lunaris, NoopConsolidator, NoopReranker, Reranker,
};
use lunaris_consolidate::{ConsolidateEvent, ConsolidationReport};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder,
};
use parking_lot::Mutex;
use serde_json::json;

// ---------------------------------------------------------------------------
// RecordingStorageWithKeyword — same shape as graph_pipeline_smoke.rs but
// renamed published-by-topic helper to "consolidate" so the assertions read
// naturally for this test file.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingStorageWithKeyword {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    chunk_ids: Mutex<Vec<Vec<u8>>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
    /// Topic + partition + payload for every `publish` call.
    published_messages: Mutex<Vec<(String, u16, Bytes)>>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }

    fn published_consolidate_count(&self) -> usize {
        self.published_messages
            .lock()
            .iter()
            .filter(|(t, _, _)| t == "__lunaris_consolidate__")
            .count()
    }

    fn published_audit_count(&self) -> usize {
        self.published_messages
            .lock()
            .iter()
            .filter(|(t, _, _)| t == "__lunaris_audit__")
            .count()
    }

    fn first_consolidate_envelope(&self) -> Option<serde_json::Value> {
        self.published_messages
            .lock()
            .iter()
            .find(|(t, _, _)| t == "__lunaris_consolidate__")
            .and_then(|(_, _, payload)| serde_json::from_slice(payload).ok())
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(&self, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
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

    // B-6 fix: vector_search has 6 params (index, query, k, filter, as_of, rerank).
    async fn vector_search(
        &self,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }

    // B-6 fix: graph_traverse takes &CypherQuery + Option<Hlc>.
    async fn graph_traverse(
        &self,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new())))
    }

    // B-6 fix: read_as_of returns Row { key, value, bt } — key field present;
    // bt uses Hlc::ZERO (NOT Hlc::default — Hlc has no Default impl).
    async fn read_as_of(
        &self,
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
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        let mut msgs = self.published_messages.lock();
        msgs.push((topic.to_string(), partition, payload));
        Ok(msgs.len() as u64)
    }

    async fn subscribe(
        &self,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        // For this smoke test we do NOT need the worker to actually receive
        // messages — we ONLY assert the pipeline TOGGLE state and the
        // publish path (D-16). Returning an empty stream lets the worker
        // spin once + idle without consuming anything; the
        // tokio::time::sleep below the spawn keeps the test deterministic.
        Ok(Box::pin(stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorageWithKeyword {
    async fn keyword_search(
        &self,
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
// W-2 — full MockConsolidator skeleton (was missing in prior plan).
// ---------------------------------------------------------------------------

/// Per-call counter so `with_consolidator` swap tests can prove the new
/// consolidator is the one being invoked.
#[derive(Default)]
struct MockConsolidator {
    call_count: AtomicU64,
}

#[async_trait]
impl Consolidator for MockConsolidator {
    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        _events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        // W-2 — return the empty per-event report shape; the smoke tests
        // exercise the toggle + publish path, not the actual ACT-R logic.
        Ok(ConsolidationReport::default())
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
    // B-6 fix — HlcClock::new(node_id) returns Arc<Self> directly.
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);
    (handle, rec, clock)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consolidate_off_default_uses_phase2_fast_path() {
    // Default-OFF posture per blueprint §5.1 + D-08.
    let (handle, _rec, _clock) = build_handle();
    assert!(
        !handle.consolidator_pipeline().is_enabled(),
        "consolidator_pipeline().is_enabled() MUST be false by default"
    );
    // Smoke check: calling enable() then disable() leaves the pipeline OFF
    // and reports two state transitions.
    let cp = handle.consolidator_pipeline();
    cp.enable();
    cp.disable();
    cp.join_worker().await;
    assert!(!cp.is_enabled());
    assert_eq!(cp.state_change_count(), 2);
}

#[tokio::test]
async fn consolidate_enable_then_ingest_publishes_event() {
    // D-16 publish-after-commit — every Lunaris::ingest publishes one
    // __lunaris_consolidate__ event, even with the pipeline OFF (the
    // pipeline ON/OFF gates the WORKER, not the publish path).
    let (handle, rec, clock) = build_handle();
    let ep = Episode::new("ingest.md", "# Notes\nThe quick brown fox.", &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    assert_eq!(
        rec.published_consolidate_count(),
        1,
        "Lunaris::ingest MUST publish exactly one __lunaris_consolidate__ event per call (D-16)"
    );

    let envelope = rec
        .first_consolidate_envelope()
        .expect("consolidate envelope must be valid JSON");
    assert_eq!(
        envelope.get("kind").and_then(|v| v.as_str()),
        Some("ingest_committed"),
        "consolidate envelope kind matches lunaris_consolidate::ConsolidateEvent verbatim"
    );
    assert!(envelope.get("episode_id").is_some(), "envelope carries episode_id");
    assert!(envelope.get("lsn_wall_ms").is_some(), "envelope carries lsn_wall_ms");
    assert!(envelope.get("lsn_counter").is_some(), "envelope carries lsn_counter");
}

/// Phase 9.1 Plan 01 Task 2 RED — publish_consolidate_event MUST populate
/// the envelope's `source` field from the ingested Episode.source at
/// publish-time. Without this, Consolidator::consolidate_scoped's filter
/// (`event.source.starts_with(prefix)`) has no data to match on and every
/// scoped WorkingMemory::consolidate pass (Plan 09.1-01 Task 3) silently
/// drops every event.
#[tokio::test]
async fn ingest_publishes_consolidate_envelope_with_episode_source() {
    let (handle, rec, clock) = build_handle();
    let ep = Episode::new(
        "test:wm/note-0",
        "# Notes\nScope-aware consolidate payload.",
        &clock,
    );
    handle.ingest(ep).await.expect("ingest must succeed");

    let envelope = rec
        .first_consolidate_envelope()
        .expect("consolidate envelope must be valid JSON");
    assert_eq!(
        envelope.get("source").and_then(|v| v.as_str()),
        Some("test:wm/note-0"),
        "publish_consolidate_event MUST copy Episode.source into the envelope \
         (Phase 9.1 Plan 01 Task 2; consolidate_scoped filters on this field)"
    );
}

#[tokio::test]
async fn ingest_publishes_one_consolidate_event_per_call() {
    // D-16 strictness — N ingests produce exactly N consolidate publishes.
    let (handle, rec, clock) = build_handle();
    for i in 0..3 {
        let ep =
            Episode::new(format!("ep{i}.md"), format!("# E{i}\nAlice and Bob."), &clock);
        handle.ingest(ep).await.expect("ingest must succeed");
    }
    assert_eq!(
        rec.published_consolidate_count(),
        3,
        "3 ingests MUST produce exactly 3 __lunaris_consolidate__ publishes"
    );
}

#[tokio::test]
async fn toggle_off_on_off_is_idempotent_and_observable() {
    // D-12 verbatim — state counter only increments on real transitions.
    let (handle, _rec, _clock) = build_handle();
    let cp = handle.consolidator_pipeline();
    assert!(!cp.is_enabled());
    assert_eq!(cp.state_change_count(), 0);

    cp.enable();
    cp.enable(); // idempotent
    assert_eq!(cp.state_change_count(), 1);

    cp.disable();
    cp.disable(); // idempotent
    assert_eq!(cp.state_change_count(), 2);

    cp.enable();
    assert!(cp.is_enabled());
    assert_eq!(
        cp.state_change_count(),
        3,
        "exactly 3 real transitions across the ON-OFF-ON sequence"
    );
    cp.disable();
    cp.join_worker().await;
}

#[tokio::test]
async fn noop_consolidator_with_pipeline_on_emits_no_audit() {
    // NoopConsolidator's applies()==false short-circuits before the audit
    // emit path can fire. With the pipeline ON and an empty subscribe
    // stream, the worker idles immediately and produces zero audit events.
    let (handle, rec, _clock) = build_handle();
    handle.consolidator_pipeline().enable();
    assert!(handle.consolidator_pipeline().is_enabled());

    // The default consolidator on with_parts_keyword is NoopConsolidator —
    // verify the assumption.
    assert!(
        !handle
            .consolidator()
            .expect("default Noop installed")
            .applies(),
        "NoopConsolidator.applies() MUST be false"
    );

    // Let the worker spin briefly. Empty subscribe stream → no events.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    handle.consolidator_pipeline().disable();
    handle.consolidator_pipeline().join_worker().await;

    assert_eq!(
        rec.published_audit_count(),
        0,
        "NoopConsolidator + empty subscribe MUST emit zero __lunaris_audit__ messages"
    );
}

#[tokio::test]
async fn with_consolidator_swaps_handle_consolidator() {
    // with_consolidator(...) replaces the installed consolidator. Subsequent
    // snapshots reflect the new instance.
    let (handle, _rec, _clock) = build_handle();
    let mock_a: Arc<dyn Consolidator> = Arc::new(MockConsolidator::default());
    let handle = handle.with_consolidator(mock_a.clone());

    let installed = handle.consolidator().expect("snapshot installed");
    assert!(
        Arc::ptr_eq(&installed, &mock_a),
        "snapshot MUST return the just-installed consolidator"
    );

    let mock_b: Arc<dyn Consolidator> = Arc::new(MockConsolidator::default());
    let handle = handle.with_consolidator(mock_b.clone());
    let installed = handle.consolidator().expect("snapshot installed");
    assert!(
        Arc::ptr_eq(&installed, &mock_b),
        "snapshot MUST return the second consolidator after the swap"
    );
    // Toggle state preserved across the swap (D-12).
    let cp = handle.consolidator_pipeline();
    assert!(!cp.is_enabled());
    assert_eq!(cp.state_change_count(), 0);
}

#[tokio::test]
async fn empty_consolidation_report_default_is_constructible() {
    // Compile-time + runtime sanity: ConsolidationReport::default() exists
    // and yields zero entries. NoopConsolidator returns this on every call.
    let r: ConsolidationReport = ConsolidationReport::default();
    assert!(r.promotions.is_empty());
    assert!(r.archives.is_empty());
    assert_eq!(r.communities_rebuilt, 0);
    // ConsolidatorPipelineHandle is reachable on the umbrella crate.
    let _: Arc<ConsolidatorPipelineHandle> =
        Arc::new(ConsolidatorPipelineHandle::with_noop());
    // NoopConsolidator is constructible from the umbrella crate.
    let _: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);
    let _ = json!({}); // serde_json import touch
}
