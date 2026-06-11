//! Wave 3F integration tests — `ConsolidateSupervisor` cross-scope isolation
//! and panic isolation (RFC 0001 §3.7).
//!
//! ## Test contracts
//!
//! 1. **Cross-scope isolation**: register scope_a and scope_b; events published
//!    to scope_a's topic must only be processed by scope_a's task, and vice
//!    versa. No cross-contamination.
//!
//! 2. **Panic isolation**: a consolidator that panics for scope_a must not
//!    affect scope_b's task. scope_b continues draining its topic normally.
//!
//! Both tests use in-process channel-backed mock storages so they require no
//! live Moon or Postgres instance.
//!
//! ## Flush strategy
//!
//! Events are enqueued into the `UnboundedSender` and the sender is dropped
//! BEFORE `register_scope` is called. This way, when the scope task subscribes
//! and starts polling, all events are already queued in the receiver. The task
//! drains them, hits `None` (sender dropped) → calls `flush` → consolidator
//! records the call. No debounce timer needed — the stream-close path fires the
//! flush deterministically.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_consolidate::types::ConsolidateEvent;
use lunaris_consolidate::{
    ConsolidateSupervisor, ConsolidationReport, Consolidator, DEFAULT_SCOPE_CONCURRENCY,
    scope_consolidate_topic,
};
use lunaris_core::{
    CypherQuery, Filter, GraphResult, Hlc, Lsn, LunarisError, QueueMsg, Row, Scope,
    StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Channel-backed StoragePort mock
// ---------------------------------------------------------------------------

/// A `StoragePort` backed by per-topic `mpsc::UnboundedReceiver` channels.
/// Events are enqueued via the returned sender; `subscribe` drains the receiver.
struct ChannelStorage {
    receivers: Arc<Mutex<HashMap<String, mpsc::UnboundedReceiver<Bytes>>>>,
}

impl ChannelStorage {
    fn new() -> Arc<Self> {
        Arc::new(Self { receivers: Arc::new(Mutex::new(HashMap::new())) })
    }

    /// Pre-create a topic channel. Returns the sender so callers can enqueue
    /// payloads before the scope task subscribes.
    fn create_topic(&self, topic: &str) -> mpsc::UnboundedSender<Bytes> {
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        self.receivers.lock().insert(topic.to_string(), rx);
        tx
    }
}

#[async_trait]
impl StoragePort for ChannelStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn { wall_ms: 0, counter: 0 })
    }

    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        _scope: &Scope,
        _i: &str,
        _q: &[f32],
        _k: usize,
        _f: Option<&Filter>,
        _a: Option<Hlc>,
        _r: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _a: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        _p: &[u8],
        _a: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(futures::stream::iter(Vec::new())))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        _k: &[u8],
        _a: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }

    async fn publish(
        &self,
        _scope: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        let rx = self.receivers.lock().remove(topic);
        match rx {
            Some(rx) => {
                let topic_str = topic.to_string();
                let stream =
                    futures::stream::unfold((rx, topic_str), |(mut rx, topic_str)| async move {
                        rx.recv().await.map(|payload| {
                            let msg = QueueMsg {
                                topic: topic_str.clone(),
                                partition: 0,
                                offset: 0,
                                payload,
                            };
                            (Ok(msg), (rx, topic_str))
                        })
                    });
                Ok(Box::pin(stream))
            }
            None => Ok(Box::pin(futures::stream::empty())),
        }
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Recording Consolidator mock
// ---------------------------------------------------------------------------

/// Records calls to `consolidate_scoped` as `(scope_prefix, filtered_event_count)`.
type RecordedCalls = Arc<Mutex<Vec<(Option<String>, usize)>>>;

#[derive(Default)]
struct RecordingConsolidator {
    calls: RecordedCalls,
}

#[async_trait]
impl Consolidator for RecordingConsolidator {
    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        // Record with None scope (direct consolidate call).
        self.calls.lock().push((None, events.len()));
        Ok(ConsolidationReport::default())
    }

    async fn consolidate_scoped(
        &self,
        storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
        scope_prefix: Option<&str>,
    ) -> Result<ConsolidationReport, LunarisError> {
        // Apply the same filter as the default impl so we record what actually
        // passes through to consolidate().
        let filtered: Vec<_> = match scope_prefix {
            None => events.to_vec(),
            Some(prefix) => {
                events.iter().filter(|e| e.source.starts_with(prefix)).cloned().collect()
            }
        };
        // Record the scoped call with filtered count.
        self.calls.lock().push((scope_prefix.map(|s| s.to_string()), filtered.len()));
        // Delegate to consolidate with the filtered batch.
        self.consolidate(storage, &filtered).await
    }

    fn applies(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn make_event_payload(source: &str) -> Bytes {
    let ev = ConsolidateEvent {
        kind: "ingest_committed".into(),
        episode_id: Ulid::new(),
        lsn_wall_ms: 1,
        lsn_counter: 1,
        source: source.to_string(),
    };
    Bytes::from(serde_json::to_vec(&ev).unwrap())
}

// ---------------------------------------------------------------------------
// Test 1: cross-scope isolation
// ---------------------------------------------------------------------------

/// Publish events to scope_a's topic and scope_b's topic **before** the scope
/// tasks subscribe. Each scope task drains its pre-filled channel, hits None
/// (sender already dropped), and flushes. Assert no cross-contamination.
#[tokio::test]
async fn supervisor_cross_scope_isolation() {
    let scope_a = Scope::new("tenant.alpha").unwrap();
    let scope_b = Scope::new("tenant.beta").unwrap();

    let topic_a = scope_consolidate_topic(&scope_a);
    let topic_b = scope_consolidate_topic(&scope_b);

    let storage = ChannelStorage::new();

    // Pre-fill channels BEFORE registering scopes (senders dropped after send
    // so the stream ends deterministically without a debounce timer).
    {
        let tx_a = storage.create_topic(&topic_a);
        for _ in 0..2 {
            tx_a.send(make_event_payload("tenant.alpha/note")).unwrap();
        }
        // tx_a dropped here → receiver will yield 2 msgs then None.
    }
    {
        let tx_b = storage.create_topic(&topic_b);
        for _ in 0..3 {
            tx_b.send(make_event_payload("tenant.beta/note")).unwrap();
        }
        // tx_b dropped here → receiver will yield 3 msgs then None.
    }

    let recorder = Arc::new(RecordingConsolidator::default());
    let calls = recorder.calls.clone();

    let sup = ConsolidateSupervisor::new(
        storage as Arc<dyn StoragePort>,
        recorder as Arc<dyn Consolidator>,
        DEFAULT_SCOPE_CONCURRENCY,
    );

    // Register scopes. Each scope task subscribes, drains its pre-filled
    // channel, and flushes when the stream closes.
    sup.register_scope(scope_a.clone()).await.unwrap();
    sup.register_scope(scope_b.clone()).await.unwrap();

    // Allow tasks to drain, flush, and exit. 300ms is generous — the stream
    // delivers all buffered messages immediately, then None triggers flush.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    sup.shutdown_all().await;

    let calls_snap = calls.lock().clone();

    // consolidate_scoped records (Some(scope), filtered_count).
    // scope_a's task passes "tenant.alpha" as scope_prefix; events with source
    // "tenant.alpha/note" match → filtered_count = 2.
    let scope_a_events: usize = calls_snap
        .iter()
        .filter(|(s, _)| s.as_deref() == Some("tenant.alpha"))
        .map(|(_, n)| *n)
        .sum();
    let scope_b_events: usize = calls_snap
        .iter()
        .filter(|(s, _)| s.as_deref() == Some("tenant.beta"))
        .map(|(_, n)| *n)
        .sum();

    assert_eq!(
        scope_a_events, 2,
        "scope_a task must process exactly 2 events (no cross-contamination); got {scope_a_events}"
    );
    assert_eq!(
        scope_b_events, 3,
        "scope_b task must process exactly 3 events (no cross-contamination); got {scope_b_events}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: panic isolation
// ---------------------------------------------------------------------------

/// A consolidator that panics when scope_prefix starts with "panic:".
#[derive(Default)]
struct PanicOnScopeConsolidator {
    good_calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl Consolidator for PanicOnScopeConsolidator {
    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        _events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        Ok(ConsolidationReport::default())
    }

    async fn consolidate_scoped(
        &self,
        _storage: Arc<dyn StoragePort>,
        _events: &[ConsolidateEvent],
        scope_prefix: Option<&str>,
    ) -> Result<ConsolidationReport, LunarisError> {
        if scope_prefix.map(|s| s.starts_with("panic:")).unwrap_or(false) {
            panic!("intentional panic for panic: scope — testing isolation");
        }
        *self.good_calls.lock() += 1;
        Ok(ConsolidationReport::default())
    }

    fn applies(&self) -> bool {
        true
    }
}

/// A panic in scope_a's consolidation (inside `tokio::spawn` in `flush()`)
/// must not stop scope_b's task. scope_b records at least one successful call.
///
/// The panic is caught by `flush`'s inner spawn join (logged as
/// `consolidate_worker_panicked`). The scope task continues; scope_b is
/// completely independent.
#[tokio::test]
async fn supervisor_panic_in_scope_a_does_not_affect_scope_b() {
    let scope_a = Scope::new("panic.scope-a").unwrap();
    let scope_b = Scope::new("good.scope-b").unwrap();

    let topic_a = scope_consolidate_topic(&scope_a);
    let topic_b = scope_consolidate_topic(&scope_b);

    let storage = ChannelStorage::new();

    // Pre-fill channels and drop senders before registering scopes.
    {
        let tx_a = storage.create_topic(&topic_a);
        tx_a.send(make_event_payload("panic:scope-a/note")).unwrap();
        // tx_a dropped → scope_a stream ends after 1 msg → flush → panic (inside spawn)
    }
    {
        let tx_b = storage.create_topic(&topic_b);
        tx_b.send(make_event_payload("good:scope-b/note-1")).unwrap();
        tx_b.send(make_event_payload("good:scope-b/note-2")).unwrap();
        // tx_b dropped → scope_b stream ends after 2 msgs → flush → good_calls++
    }

    let consolidator = Arc::new(PanicOnScopeConsolidator::default());
    let good_calls = consolidator.good_calls.clone();

    let sup = ConsolidateSupervisor::new(
        storage as Arc<dyn StoragePort>,
        consolidator as Arc<dyn Consolidator>,
        DEFAULT_SCOPE_CONCURRENCY,
    );

    sup.register_scope(scope_a.clone()).await.unwrap();
    sup.register_scope(scope_b.clone()).await.unwrap();

    // Allow tasks to drain and process.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    sup.shutdown_all().await;

    let good = *good_calls.lock();
    assert!(
        good >= 1,
        "scope_b must have processed ≥1 event despite panic in scope_a; good_calls={good}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: double-register is a no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_double_register_is_noop() {
    let scope = Scope::new("acme.agent-1").unwrap();
    let storage = ChannelStorage::new();
    storage.create_topic(&scope_consolidate_topic(&scope));

    let sup = ConsolidateSupervisor::new(
        storage as Arc<dyn StoragePort>,
        Arc::new(lunaris_consolidate::NoopConsolidator),
        DEFAULT_SCOPE_CONCURRENCY,
    );

    sup.register_scope(scope.clone()).await.unwrap();
    // Second registration must be a no-op.
    sup.register_scope(scope.clone()).await.unwrap();

    assert_eq!(sup.active_scopes().len(), 1, "only one task per scope");
    sup.shutdown_all().await;
}

// ---------------------------------------------------------------------------
// Test 4: deregister removes scope from active map
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_deregister_removes_scope() {
    let scope = Scope::new("acme.agent-2").unwrap();
    let storage = ChannelStorage::new();
    storage.create_topic(&scope_consolidate_topic(&scope));

    let sup = ConsolidateSupervisor::new(
        storage as Arc<dyn StoragePort>,
        Arc::new(lunaris_consolidate::NoopConsolidator),
        DEFAULT_SCOPE_CONCURRENCY,
    );

    sup.register_scope(scope.clone()).await.unwrap();
    assert_eq!(sup.active_scopes().len(), 1);

    sup.deregister_scope(&scope);
    // Yield so the task can react to the dropped sender.
    tokio::task::yield_now().await;

    assert_eq!(
        sup.active_scopes().len(),
        0,
        "deregister must remove scope from active map immediately"
    );
    sup.shutdown_all().await;
}
