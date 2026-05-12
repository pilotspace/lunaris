//! Phase 9.1 Plan 01 Task 3 — `WorkingMemory::consolidate` scope-isolation
//! tests.
//!
//! Lives here (NOT inline in `working_memory.rs`) per CONTEXT.md line 170
//! Claude's Discretion note — the `StoragePort` test double with a
//! `publish → subscribe` mpsc bridge (so the `consolidate()` drain sees what
//! the test publishes) plus the `TestConsolidator` fixture runs ~250 LOC of
//! machinery that would balloon the production module past any reasonable
//! size for pure-logic unit tests. The primitive module stays lean; the
//! harness lives alongside the parity-test files.
//!
//! Four tests:
//! 1. `wm_consolidate_scope_isolation` — 10 seeded events (5 `test:wm/`,
//!    5 `other:`); asserts `report.promotions.len() == 5` AND every
//!    `episode_id` maps back to a `test:wm/`-scoped source.
//! 2. `wm_consolidate_publishes_audit_event_per_promotion` — same fixture;
//!    asserts exactly 5 `AuditEvent::ConsolidatorPromotion` payloads land
//!    on `__lunaris_audit__`.
//! 3. `wm_consolidate_with_empty_queue_returns_empty_report` — no events
//!    published; `.consolidate()` returns empty report, zero audits.
//! 4. `wm_consolidate_noop_consolidator_returns_empty_report` — default
//!    `NoopConsolidator` path; `.consolidate()` returns empty report.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::{AuditEvent, Consolidator, Lunaris, NoopReranker, Reranker};
use lunaris_consolidate::{
    CONSOLIDATE_TOPIC, ConsolidateEvent, ConsolidationReport, FactId, PromotionEvent,
};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder,
};
use lunaris_recipes::WorkingMemory;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// BridgedStorage — publish/subscribe bridge so WorkingMemory::consolidate's
// drain actually sees the events the test seeds via storage.publish.
// Mirrors the pattern in `crates/lunaris/tests/verify_pipeline_smoke.rs`.
// ---------------------------------------------------------------------------

struct BridgedStorage {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// All published messages by (topic, partition, payload).
    published: Mutex<Vec<(String, u16, Bytes)>>,
    /// Per-topic mpsc sender; `subscribe` takes the matching receiver out
    /// of `subscribe_rx` on first call and drives a `stream::unfold`.
    #[allow(clippy::type_complexity)]
    consolidate_tx: Mutex<Option<mpsc::UnboundedSender<QueueMsg>>>,
    consolidate_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<QueueMsg>>>,
}

impl BridgedStorage {
    fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<QueueMsg>();
        Arc::new(Self {
            rows: Mutex::new(HashMap::new()),
            published: Mutex::new(Vec::new()),
            consolidate_tx: Mutex::new(Some(tx)),
            consolidate_rx: tokio::sync::Mutex::new(Some(rx)),
        })
    }

    fn audit_promotion_count(&self) -> usize {
        self.published
            .lock()
            .iter()
            .filter(|(t, _, p)| {
                t == "__lunaris_audit__"
                    && serde_json::from_slice::<serde_json::Value>(p)
                        .ok()
                        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
                        .as_deref()
                        == Some("ConsolidatorPromotion")
            })
            .count()
    }

    fn audit_events(&self) -> Vec<AuditEvent> {
        self.published
            .lock()
            .iter()
            .filter(|(t, _, _)| t == "__lunaris_audit__")
            .filter_map(|(_, _, p)| serde_json::from_slice::<AuditEvent>(p).ok())
            .collect()
    }
}

#[async_trait]
impl StoragePort for BridgedStorage {
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
        Ok(Box::pin(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new())))
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
        // Store every published message for inspection.
        let mut pubs = self.published.lock();
        pubs.push((topic.to_string(), partition, payload.clone()));
        let offset = pubs.len() as u64;
        drop(pubs);

        // Bridge publishes on __lunaris_consolidate__ into the subscribe
        // mpsc so WorkingMemory::consolidate's drain can see them.
        if topic == CONSOLIDATE_TOPIC
            && let Some(tx) = self.consolidate_tx.lock().as_ref()
        {
            let _ = tx.send(QueueMsg { topic: topic.to_string(), partition, offset, payload });
        }
        Ok(offset)
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        if topic == CONSOLIDATE_TOPIC {
            let mut slot = self.consolidate_rx.lock().await;
            match slot.take() {
                Some(rx) => {
                    let s = stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|msg| (Ok(msg), rx))
                    });
                    Ok(Box::pin(s))
                }
                None => Ok(Box::pin(stream::empty())),
            }
        } else {
            Ok(Box::pin(stream::empty()))
        }
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
        }
    }
}

#[async_trait]
impl KeywordPort for BridgedStorage {
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
// TestConsolidator — returns one PromotionEvent per input event; fact_id
// derived deterministically from episode_id so tests can correlate.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TestConsolidator {
    call_count: AtomicU64,
}

#[async_trait]
impl Consolidator for TestConsolidator {
    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        // Build a deterministic FactId for each event from its episode_id
        // bytes, so the audit-count test can correlate without additional
        // state. The predicate `"promoted"` is arbitrary — FactId is the
        // blake3 hash of (subject || predicate || object); we just need
        // it to be well-formed.
        use lunaris_extract::EntityId;
        let promotions = events
            .iter()
            .map(|e| {
                let id_bytes = e.episode_id.to_bytes();
                let mut subject = [0u8; 16];
                subject.copy_from_slice(&id_bytes);
                PromotionEvent {
                    episode_id: e.episode_id,
                    fact_id: FactId::from_triple(
                        EntityId(subject),
                        "promoted",
                        EntityId([2u8; 16]),
                    ),
                    activation_score: 1.0,
                }
            })
            .collect();
        Ok(ConsolidationReport { promotions, archives: vec![], communities_rebuilt: 0 })
    }

    fn applies(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_handle() -> (Lunaris, Arc<BridgedStorage>, Arc<HlcClock>) {
    let rec = BridgedStorage::new();
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);
    (handle, rec, clock)
}

async fn seed_events(
    storage: &Arc<dyn StoragePort>,
    scoped_count: usize,
    other_count: usize,
) -> Vec<Ulid> {
    let mut ids = Vec::new();
    for i in 0..scoped_count {
        let ep_id = Ulid::new();
        ids.push(ep_id);
        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: ep_id,
            lsn_wall_ms: i as u64,
            lsn_counter: 0,
            source: format!("test:wm/note-{i}"),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        storage
            .publish(&lunaris_core::Scope::dev(), CONSOLIDATE_TOPIC, 0, payload.into())
            .await
            .unwrap();
    }
    for i in 0..other_count {
        let ep_id = Ulid::new();
        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: ep_id,
            lsn_wall_ms: (scoped_count + i) as u64,
            lsn_counter: 0,
            source: format!("other:note-{i}"),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        storage
            .publish(&lunaris_core::Scope::dev(), CONSOLIDATE_TOPIC, 0, payload.into())
            .await
            .unwrap();
    }
    ids
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wm_consolidate_scope_isolation() {
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    let storage: Arc<dyn StoragePort> = rec.clone();
    let scoped_ids = seed_events(&storage, 5, 5).await;

    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let report = wm.consolidate().await.expect("consolidate must succeed");

    assert_eq!(
        report.promotions.len(),
        5,
        "exactly 5 of 10 seeded events match scope_prefix \"test:wm/\" \
         (5 scoped + 5 other); got {}",
        report.promotions.len()
    );

    let promoted_ids: Vec<Ulid> = report.promotions.iter().map(|p| p.episode_id).collect();
    for id in &scoped_ids {
        assert!(
            promoted_ids.contains(id),
            "every scoped episode_id MUST appear in promotions; missing {id}"
        );
    }
}

#[tokio::test]
async fn wm_consolidate_publishes_audit_event_per_promotion() {
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    let storage: Arc<dyn StoragePort> = rec.clone();
    let _scoped_ids = seed_events(&storage, 5, 5).await;

    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let report = wm.consolidate().await.expect("consolidate must succeed");
    assert_eq!(report.promotions.len(), 5);

    assert_eq!(
        rec.audit_promotion_count(),
        5,
        "each PromotionEvent MUST produce exactly one \
         AuditEvent::ConsolidatorPromotion on __lunaris_audit__"
    );
    let audits = rec.audit_events();
    let promotion_events: Vec<_> =
        audits.iter().filter(|e| matches!(e, AuditEvent::ConsolidatorPromotion { .. })).collect();
    assert_eq!(promotion_events.len(), 5);
}

#[tokio::test]
async fn wm_consolidate_with_empty_queue_returns_empty_report() {
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let report = wm.consolidate().await.expect("consolidate on empty queue must succeed");
    assert!(report.promotions.is_empty(), "empty queue → empty promotions");
    assert!(report.archives.is_empty(), "empty queue → empty archives");
    assert_eq!(rec.audit_promotion_count(), 0, "no promotions → no audit events");
}

#[tokio::test]
async fn wm_consolidate_noop_consolidator_returns_empty_report() {
    // Default pipeline installs NoopConsolidator — no set_consolidator call.
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);

    let storage: Arc<dyn StoragePort> = rec.clone();
    let _ = seed_events(&storage, 5, 5).await;

    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let report = wm.consolidate().await.expect("noop consolidate must succeed");
    assert!(report.promotions.is_empty(), "NoopConsolidator returns empty report for any input");
    assert_eq!(
        rec.audit_promotion_count(),
        0,
        "NoopConsolidator produces zero promotions → zero audits"
    );
}
