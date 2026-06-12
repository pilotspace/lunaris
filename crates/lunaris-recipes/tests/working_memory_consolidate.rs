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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
            // Non-destructive snapshot: borrow the receiver mutably, drain all
            // currently-buffered messages via try_recv, then PUT THE RECEIVER
            // BACK (never take() it out of the slot).  Returning a finite
            // stream::iter means:
            //
            //  • drain_consolidate_events breaks on Ok(None) (stream end) — a
            //    finite snapshot is correct for the production drain loop.
            //  • Events published AFTER this subscribe call (e.g. the future
            //    re-queue of foreign events inside consolidate_scoped) buffer
            //    in the unbounded channel and surface on the NEXT subscribe
            //    call.  That is precisely the semantics the discriminating
            //    tests need: a second consolidate_unfiltered() call drains the
            //    re-queued foreign events correctly.
            //  • The old take()-semantics consumed the receiver on the first
            //    call and returned stream::empty() forever after — making both
            //    discriminating tests unsatisfiable (red for the wrong reason).
            let mut slot = self.consolidate_rx.lock().await;
            let msgs: Vec<QueueMsg> = if let Some(ref mut rx) = *slot {
                let mut buf = Vec::new();
                loop {
                    match rx.try_recv() {
                        Ok(msg) => buf.push(msg),
                        Err(_) => break,
                    }
                }
                buf
            } else {
                Vec::new()
            };
            // slot (and the receiver inside it) is dropped here — lock released.
            Ok(Box::pin(stream::iter(msgs.into_iter().map(Ok))))
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
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
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

// ============================================================================
// RED SUITE — consolidate-prefix-drop (ADD task, 2026-06-12)
//
// These four tests pin the behaviour described in
// .add/tasks/consolidate-prefix-drop/TASK.md §4.
//
// BUG (red reason):
//   WorkingMemory::consolidate calls drain_consolidate_events, which
//   subscribes to CONSOLIDATE_TOPIC and consumes ALL events for the scope
//   from the queue.  It then calls consolidate_scoped(…, Some(prefix)),
//   whose default impl FILTERS the already-drained batch to matching events
//   only and silently discards the rest.  The non-matching ("foreign")
//   events are gone — consumed from the single consumer-group queue and
//   never re-queued.  Any subsequent consolidate_unfiltered() (or a second
//   consolidate pass for the foreign namespace) finds an empty queue.
// ============================================================================

// ---------------------------------------------------------------------------
// FailingPublishStorage — wraps BridgedStorage and delegates all StoragePort
// methods to it, but returns Err from publish once fail_publish is set.
// Used by republish_failure_is_loud_not_fatal to prove the fix handles a
// failed re-queue loudly rather than silently.
// ---------------------------------------------------------------------------

struct FailingPublishStorage {
    inner: Arc<BridgedStorage>,
    /// Flip to `true` after seeding to make subsequent publish calls fail.
    fail_publish: AtomicBool,
}

impl FailingPublishStorage {
    fn new(inner: Arc<BridgedStorage>) -> Arc<Self> {
        Arc::new(Self { inner, fail_publish: AtomicBool::new(false) })
    }

    /// After calling this, every publish invocation returns
    /// `StorageError::Backend("injected publish failure")`.
    fn arm(&self) {
        self.fail_publish.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl StoragePort for FailingPublishStorage {
    async fn atomic_write(
        &self,
        scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        self.inner.atomic_write(scope, ops).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        scope: &lunaris_core::Scope,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        self.inner.vector_search(scope, index, query, k, filter, as_of, rerank).await
    }

    async fn graph_traverse(
        &self,
        scope: &lunaris_core::Scope,
        q: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        self.inner.graph_traverse(scope, q, as_of).await
    }

    async fn scan_range(
        &self,
        scope: &lunaris_core::Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(bytes::Bytes, bytes::Bytes), StorageError>>, StorageError>
    {
        self.inner.scan_range(scope, prefix, as_of).await
    }

    async fn read_as_of(
        &self,
        scope: &lunaris_core::Scope,
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        self.inner.read_as_of(scope, key, as_of).await
    }

    async fn publish(
        &self,
        scope: &lunaris_core::Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        // Always forward to inner FIRST so that inner.published records EVERY
        // attempt (including the re-queue attempts made by the fix).  Only then
        // override the return value with Err when fail_publish is armed.
        //
        // This is the second harness satisfiability fix (companion to the
        // subscribe snapshot fix above).  Without it, the discriminating
        // assertion in `republish_failure_is_loud_not_fatal`
        //   assert_eq!(consolidate_publishes, 15)
        // could never go green: the wrapper short-circuited before calling
        // inner.publish, so inner.published stayed at 10 regardless of whether
        // the production fix made its 5 re-queue attempts.
        let result = self.inner.publish(scope, topic, partition, payload).await;
        if self.fail_publish.load(Ordering::SeqCst) {
            Err(StorageError::Backend("injected publish failure".into()))
        } else {
            result
        }
    }

    async fn subscribe(
        &self,
        scope: &lunaris_core::Scope,
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        self.inner.subscribe(scope, group, topic, partition).await
    }

    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }
}

#[async_trait]
impl KeywordPort for FailingPublishStorage {
    async fn keyword_search(
        &self,
        scope: &lunaris_core::Scope,
        index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        self.inner.keyword_search(scope, index, query, k, filter, as_of).await
    }
}

/// Build a Lunaris handle backed by `FailingPublishStorage` wrapping a fresh
/// `BridgedStorage`.  Returns the handle, a ref to the wrapper (so tests can
/// call `arm()`), and the inner `BridgedStorage` (for audit inspection).
fn build_handle_failing_publish() -> (Lunaris, Arc<FailingPublishStorage>, Arc<BridgedStorage>) {
    let inner = BridgedStorage::new();
    let wrapper = FailingPublishStorage::new(inner.clone());
    let storage: Arc<dyn StoragePort> = wrapper.clone();
    let keyword: Arc<dyn KeywordPort> = wrapper.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock)
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);
    (handle, wrapper, inner)
}

// ---------------------------------------------------------------------------
// Test 1 — DISCRIMINATING RED
//
// Seed 5 matching ("test:wm/") + 5 foreign ("other:") events.
// Run the namespaced consolidate() — expects 5 promotions (matching only).
// Then run consolidate_unfiltered() — expects 5 promotions from the foreign
// events that should have been re-queued.
//
// TODAY THIS FAILS: the foreign events are consumed from the queue by the
// first drain_consolidate_events call and are never re-queued.  The second
// consolidate_unfiltered() finds an empty stream and returns 0 promotions.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn namespaced_consolidate_preserves_foreign_backlog() {
    // ── Arrange ─────────────────────────────────────────────────────────────
    // First handle / storage pair: namespaced consolidate pass.
    // BridgedStorage only gives out the MPSC receiver once; after the first
    // subscribe/drain the channel is exhausted.  We model the "re-queued
    // foreign events are available for a subsequent drain" expectation by
    // using a SECOND BridgedStorage that the fixed implementation would
    // publish the foreign events into.  Until the fix lands, we assert the
    // behavior we WANT and watch it fail.
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    let storage: Arc<dyn StoragePort> = rec.clone();
    let foreign_ids: Vec<Ulid> = {
        // Seed scoped events (these are returned by seed_events as ids).
        // We also need the foreign episode IDs for the second-pass assertion.
        let mut all_foreign = Vec::new();
        // seed_events publishes `scoped_count` events with "test:wm/" prefix
        // and `other_count` with "other:" prefix.  We capture foreign IDs
        // ourselves here so we can assert they appear in the second pass.
        let _scoped_ids = seed_events(&storage, 5, 0).await;
        for i in 0..5usize {
            let ep_id = Ulid::new();
            all_foreign.push(ep_id);
            let ev = ConsolidateEvent {
                kind: "ingest_committed".into(),
                episode_id: ep_id,
                lsn_wall_ms: (100 + i) as u64,
                lsn_counter: 0,
                source: format!("other:note-{i}"),
            };
            let payload = serde_json::to_vec(&ev).unwrap();
            storage
                .publish(&lunaris_core::Scope::dev(), CONSOLIDATE_TOPIC, 0, payload.into())
                .await
                .unwrap();
        }
        all_foreign
    };

    // ── Act 1: namespaced consolidate ────────────────────────────────────────
    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let first_report = wm.consolidate().await.expect("namespaced consolidate must succeed");

    // ── Assert 1: only matching events were promoted ─────────────────────────
    assert_eq!(
        first_report.promotions.len(),
        5,
        "namespaced consolidate must promote exactly the 5 matching events; \
         got {}",
        first_report.promotions.len(),
    );

    // ── Act 2: unfiltered second pass to drain re-queued foreign events ──────
    // After the fix, WorkingMemory::consolidate must re-publish the 5 foreign
    // events back to CONSOLIDATE_TOPIC so a subsequent pass can pick them up.
    // We build a SECOND handle backed by a SECOND BridgedStorage and publish
    // the foreign events into it — this simulates what the fixed re-queue path
    // would make available to consolidate_unfiltered().
    //
    // Until the fix lands, the re-queue never happens and this second storage
    // is empty → consolidate_unfiltered() returns 0 promotions → FAILS.
    let (handle2, rec2, _clock2) = build_handle();
    let handle2 = Arc::new(handle2);
    handle2.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));
    let storage2: Arc<dyn StoragePort> = rec2.clone();

    // Simulate "re-queued foreign events now visible to the second pass".
    // THE FIX would have published these back during the first consolidate().
    // Before the fix, nothing was published — so we assert the expected count
    // and the test fails for the right reason.
    //
    // INTENTIONALLY NOT publishing into storage2 here — the fix is supposed
    // to do that automatically inside consolidate().  We assert the count
    // against the ACTUAL re-queued state from the FIRST storage, proving the
    // production code must re-queue them.

    // What we actually assert: a fresh unfiltered pass over the same storage
    // that held the original mixed backlog should find the foreign events.
    // Because the first consolidate() drained the queue entirely without
    // re-queuing, this is empty today.
    let wm_unfiltered = WorkingMemory::new(handle2.clone(), lunaris_core::Scope::dev(), "other:");
    // Publish the foreign events into storage2 — representing what the fix
    // would have put back into the ORIGINAL queue.  To make this test truly
    // discriminating, we assert that the total consolidated across BOTH passes
    // equals the total seeded (10), i.e. no events were lost.
    //
    // Without the fix: first_report.promotions.len() == 5 (matching only),
    // second_report.promotions.len() == 0 (foreign lost), total == 5 ≠ 10.
    //
    // With the fix: foreign events are re-queued after the first pass,
    // second pass picks them up → total == 10.
    //
    // We seed the foreign events into storage2 to model the fixed re-queue,
    // then assert the second pass produces 5 promotions.  Since the fix is
    // NOT in place, we instead publish into storage2 explicitly and assert
    // it works — but the key discriminating assertion is the TOTAL check
    // against the ORIGINAL storage's second pass returning 0.

    // Discriminating assertion: run consolidate_unfiltered on the ORIGINAL
    // handle/storage.  Since the queue was fully drained on the first pass
    // and the fix is not in place, this returns 0 promotions.
    // We assert the total == 10 which WILL FAIL today (total == 5).
    let wm_orig_unfiltered =
        WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "other:");
    let second_report =
        wm_orig_unfiltered.consolidate_unfiltered().await.expect("second pass must succeed");

    let total_promoted = first_report.promotions.len() + second_report.promotions.len();
    assert_eq!(
        total_promoted, 10,
        "DISCRIMINATING RED: total promoted across namespaced + unfiltered passes must equal \
         total seeded (10 = 5 matching + 5 foreign). \
         Got first={}, second={} (foreign events consumed from queue and not re-queued → loss). \
         Fix: re-queue non-matching drained events inside consolidate_scoped path.",
        first_report.promotions.len(),
        second_report.promotions.len(),
    );

    // Also verify the foreign episode IDs are represented in the second pass.
    let second_ids: Vec<Ulid> =
        second_report.promotions.iter().map(|p| p.episode_id).collect();
    for id in &foreign_ids {
        assert!(
            second_ids.contains(id),
            "foreign episode_id {id} must appear in second-pass promotions after fix"
        );
    }

    // Cleanup second handle (avoids leaking into storage2 which we don't use
    // for a discriminating assertion here).
    drop(wm_unfiltered);
    drop(storage2);
}

// ---------------------------------------------------------------------------
// Test 2 — REGRESSION PIN (expected to pass today)
//
// Seed only matching events; consolidate twice with the prefix.
// Second report must be empty (drains nothing on the second pass).
//
// This pins that the fix does NOT re-publish matching (already-consolidated)
// events back to the queue, which would cause double-consolidation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matching_events_not_republished() {
    // ── Arrange ─────────────────────────────────────────────────────────────
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    let storage: Arc<dyn StoragePort> = rec.clone();
    // Seed only matching events — no foreign events.
    let _scoped_ids = seed_events(&storage, 5, 0).await;

    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");

    // ── Act 1: first consolidate ─────────────────────────────────────────────
    let first_report = wm.consolidate().await.expect("first consolidate must succeed");
    assert_eq!(
        first_report.promotions.len(),
        5,
        "first pass must promote all 5 matching events; got {}",
        first_report.promotions.len(),
    );

    // ── Act 2: second consolidate over same (now empty) queue ────────────────
    // BridgedStorage's MPSC receiver is already consumed; the second subscribe
    // returns an empty stream immediately.  The second consolidate must return
    // an empty report — matching events must NOT be re-queued.
    let second_report = wm.consolidate().await.expect("second consolidate must succeed");

    // ── Assert ───────────────────────────────────────────────────────────────
    assert!(
        second_report.promotions.is_empty(),
        "second consolidate over already-drained queue must return 0 promotions \
         (matching events must NOT be re-published); got {}",
        second_report.promotions.len(),
    );
    assert!(
        second_report.archives.is_empty(),
        "second consolidate must return 0 archives; got {}",
        second_report.archives.len(),
    );
    assert_eq!(
        rec.audit_promotion_count(),
        5,
        "total audit promotions across both passes must equal 5 (no duplicates); \
         got {}",
        rec.audit_promotion_count(),
    );
}

// ---------------------------------------------------------------------------
// Test 3 — RED (via foreign-loss assertion; loud-not-fatal is the future pin)
//
// Wrap storage with FailingPublishStorage.  After seeding, arm the failure
// flag so all subsequent publish calls return Err.
//
// Contract (frozen, from TASK.md §3):
//   - When re-queuing a foreign event fails, consolidate() must still return
//     Ok with the matching-events report (loud: callers can observe the error
//     via the report or a warning; not fatal: the consolidated matching work
//     is not lost).
//
// Today this test is red via the SAME foreign-event-loss assertion as test 1:
// the production code never attempts to re-queue foreign events, so both the
// loud-not-fatal path AND the backlog-preservation are absent.
//
// The "Ok + matching report survives a publish Err" assertion is a future
// regression guard that will become green only after the fix is in place.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn republish_failure_is_loud_not_fatal() {
    // ── Arrange ─────────────────────────────────────────────────────────────
    let (handle, wrapper, inner) = build_handle_failing_publish();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    // Seed via the wrapper while publish is NOT yet armed (pass-through).
    let storage_for_seed: Arc<dyn StoragePort> = wrapper.clone();
    let _scoped_ids = seed_events(&storage_for_seed, 5, 0).await;
    // Seed 5 foreign events directly via the wrapper (publish not yet armed).
    for i in 0..5usize {
        let ep_id = Ulid::new();
        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: ep_id,
            lsn_wall_ms: (200 + i) as u64,
            lsn_counter: 0,
            source: format!("other:note-{i}"),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        wrapper
            .publish(&lunaris_core::Scope::dev(), CONSOLIDATE_TOPIC, 0, payload.into())
            .await
            .unwrap();
    }

    // Arm the failure flag AFTER seeding so the consolidate() re-queue path
    // (which does not exist yet) would be the first caller to hit the Err.
    wrapper.arm();

    // ── Act ──────────────────────────────────────────────────────────────────
    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let result = wm.consolidate().await;

    // ── Assert: loud-not-fatal — Ok with matching promotions ────────────────
    // The fix must: attempt to re-publish the 5 foreign events; when publish
    // returns Err, surface the failure in the report (or as a warning) but
    // NOT propagate it as an Err return — the 5 matching promotions already
    // happened and must not be discarded.
    //
    // TODAY: consolidate() returns Ok(report with 5 promotions) because it
    // never attempts the re-publish at all.  The loud-not-fatal assertion
    // vacuously passes.  The discriminating red comes from the BACKLOG
    // assertion below.
    assert!(
        result.is_ok(),
        "consolidate() must return Ok even when re-queue publish fails; got {:?}",
        result.err(),
    );
    let report = result.unwrap();
    assert_eq!(
        report.promotions.len(),
        5,
        "matching promotions must be preserved even when re-queue publish fails; \
         got {}",
        report.promotions.len(),
    );

    // ── Discriminating red (same root cause as test 1) ───────────────────────
    // After the fix: foreign events would have been re-queued (or attempted
    // with a failure recorded).  We verify the inner storage received the
    // re-queue publish attempts (5 extra publishes on CONSOLIDATE_TOPIC beyond
    // the initial 10 seeded).
    //
    // WITHOUT the fix: only 10 publishes total (the 5+5 seeded ones).
    // WITH the fix + armed failure: 15 publish attempts (10 seed + 5 re-queue
    // attempts that return Err, but the attempt IS made).
    //
    // The inner BridgedStorage records ALL publish calls including failed ones
    // on the wrapper side — but since the wrapper returns Err without calling
    // inner.publish, the count stays at 10.  We assert 15 to make this red.
    let consolidate_publishes = inner
        .published
        .lock()
        .iter()
        .filter(|(t, _, _)| t == CONSOLIDATE_TOPIC)
        .count();
    assert_eq!(
        consolidate_publishes,
        15,
        "DISCRIMINATING RED (loud-not-fatal): fix must attempt to re-publish 5 foreign \
         events back to CONSOLIDATE_TOPIC (even when publish returns Err — the attempt \
         must be made and the error surfaced).  \
         Expected 10 seeded + 5 re-queue attempts = 15; got {} \
         (foreign events are currently discarded without any re-publish attempt).",
        consolidate_publishes,
    );
}

// ---------------------------------------------------------------------------
// Test 4 — REGRESSION PIN (expected to pass today)
//
// Seed mixed backlog; consolidate_unfiltered() consolidates ALL of it in one
// call.  Confirms the unfiltered path is not broken by the prefix-drop fix.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unfiltered_path_untouched() {
    // ── Arrange ─────────────────────────────────────────────────────────────
    let (handle, rec, _clock) = build_handle();
    let handle = Arc::new(handle);
    handle.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator::default()));

    let storage: Arc<dyn StoragePort> = rec.clone();
    // Seed 5 matching + 5 foreign — total 10.
    let _scoped_ids = seed_events(&storage, 5, 5).await;

    // ── Act ──────────────────────────────────────────────────────────────────
    let wm = WorkingMemory::new(handle.clone(), lunaris_core::Scope::dev(), "test:wm/");
    let report = wm.consolidate_unfiltered().await.expect("consolidate_unfiltered must succeed");

    // ── Assert ───────────────────────────────────────────────────────────────
    assert_eq!(
        report.promotions.len(),
        10,
        "consolidate_unfiltered() must consolidate ALL 10 seeded events \
         (5 matching + 5 foreign); got {}",
        report.promotions.len(),
    );
    assert_eq!(
        rec.audit_promotion_count(),
        10,
        "each of the 10 promotions must emit one audit event; got {}",
        rec.audit_promotion_count(),
    );
}
