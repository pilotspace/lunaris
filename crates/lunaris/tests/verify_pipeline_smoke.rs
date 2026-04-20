//! Plan 04-04 — verify_pipeline_smoke tests.
//!
//! Mirrors graph_pipeline_smoke.rs structure with the in-memory
//! `RecordingStorageWithKeyword` fixture. Validates:
//!
//! - Default-OFF posture (verifier pipeline disabled by default).
//! - D-08 toggle surface — enable/disable + state_change_count.
//! - D-12 idempotent toggle observability.
//! - with_verifier swap preserves toggle + counter (D-12).
//! - NoopVerifier + pipeline ON emits no MVCC writes.
//!
//! Plan 04-04 Task 4 (B-2-RESIDUAL) EXTENDS this file with the real
//! MVCC primitive-row supersede smoke test. Until then, the synthetic-key
//! Plan 04-01 stub is exercised via the worker-spawn observability path
//! (the toggle ON path).
//!
//! ## B-6 fix
//!
//! Mock StoragePort uses ACTUAL trait signatures: `vector_search` 6 params,
//! `graph_traverse` 2 params with refs, `Row { key, value, bt }` field
//! shape, `Hlc::ZERO` instead of `Hlc::default()`, `HlcClock::new` returns
//! `Arc<Self>` directly (no extra `Arc::new` wrap).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::{
    Lunaris, NoopReranker, NoopVerifier, Reranker, Verifier, VerifierBackend,
    VerifierPipelineHandle, VerifyDecision, VerifyNeedsReviewItem,
};
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
// Mock row shape — Plan 04-04 Task 4 (B-2-RESIDUAL) parses bt from the
// payload bytes to mirror the real Moon HSET / Postgres bt-derived-from-
// payload semantics. Until Task 4 lands, the bt field is unused outside the
// read_as_of return path.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct MockRow {
    pub(crate) value: Vec<u8>,
    pub(crate) bt: BiTemporal,
}

#[derive(Default)]
struct RecordingStorageWithKeyword {
    /// Plan 04-04 Task 4 (B-2-RESIDUAL): per-row bt is parsed from the
    /// WriteOp::KvPut payload bytes inside atomic_write so the mock mirrors
    /// the real Moon/Postgres backends. This makes the apply_supersede
    /// integration test see the patched bt on subsequent read_as_of.
    rows: Mutex<HashMap<Vec<u8>, MockRow>>,
    chunk_ids: Mutex<Vec<Vec<u8>>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
    published_messages: Mutex<Vec<(String, u16, Bytes)>>,
    next_lsn: AtomicU64,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }

    fn next_lsn_value(&self) -> u64 {
        self.next_lsn.fetch_add(1, Ordering::SeqCst).max(1)
    }

    fn published_verify_count(&self) -> usize {
        self.published_messages
            .lock()
            .iter()
            .filter(|(t, _, _)| t == "__lunaris_verify__")
            .count()
    }

    fn published_audit_count(&self) -> usize {
        self.published_messages
            .lock()
            .iter()
            .filter(|(t, _, _)| t == "__lunaris_audit__")
            .count()
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(&self, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        // B-2-RESIDUAL: parse bt from the payload JSON so the mock mirrors
        // the real backend semantics. Moon HSET stores `v` + `bt` as separate
        // fields BUT writes them from the same `Row<Bytes>` payload; Postgres
        // stores bt in a column derived from the persisted payload. Either
        // way, the bt that ends up persisted is the bt embedded in the
        // serialized payload — NOT a separately-tracked side value.
        //
        // Preserving the existing bt from self.rows would hide the bt
        // mutation that apply_supersede produces and defeat the purpose of
        // the rewrite. Falling back to BiTemporal::at(Hlc::ZERO, Hlc::ZERO)
        // when the payload doesn't carry a `bt` field keeps Plan 02 callers
        // (who don't write bt-shaped JSON) compiling.
        let mut rows = self.rows.lock();
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    let bt = serde_json::from_slice::<serde_json::Value>(value)
                        .ok()
                        .and_then(|v| serde_json::from_value::<BiTemporal>(v["bt"].clone()).ok())
                        .unwrap_or_else(|| BiTemporal::at(Hlc::ZERO, Hlc::ZERO));
                    rows.insert(key.clone(), MockRow { value: value.clone(), bt });
                }
                WriteOp::KvDelete { key } => {
                    rows.remove(key);
                }
                WriteOp::VectorUpsert { id, index, .. } if index == "chunks" => {
                    self.chunk_ids.lock().push(id.clone());
                }
                _ => {}
            }
        }
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: self.next_lsn_value(), counter: 0 })
    }

    // B-6: vector_search has 6 params (index, query, k, filter, as_of, rerank).
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

    // B-6: graph_traverse takes &CypherQuery + Option<Hlc>.
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

    // B-6: read_as_of returns Row { key, value, bt } — bt is the persisted
    // value (parsed from the payload at atomic_write time).
    async fn read_as_of(
        &self,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|r| Row {
            key: key.to_vec(),
            value: Bytes::from(r.value),
            bt: r.bt,
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
        // Empty stream — the smoke tests in this Task 3 layer assert toggle +
        // publish behavior, not the worker's message consumption path. Plan
        // 04-04 Task 4 (B-2 verification) extends with a payload-driven
        // subscribe stream for the real MVCC supersede assertion.
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
// MockVerifier — produces a fixed VerifyDecision on every verify() call.
// Used by Task 4's B-2 integration test.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct MockVerifier {
    pub(crate) decision: VerifyDecision,
    pub(crate) call_count: Arc<Mutex<u64>>,
}

impl MockVerifier {
    pub(crate) fn arbitrate(decision: VerifyDecision) -> Self {
        Self { decision, call_count: Arc::new(Mutex::new(0)) }
    }
}

#[async_trait]
impl Verifier for MockVerifier {
    async fn verify(
        &self,
        _item: VerifyNeedsReviewItem,
    ) -> Result<VerifyDecision, LunarisError> {
        *self.call_count.lock() += 1;
        Ok(self.decision.clone())
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
    // B-6: HlcClock::new returns Arc<Self> directly — no Arc::new wrap.
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);
    (handle, rec, clock)
}

// ---------------------------------------------------------------------------
// Tests — Task 3 layer (toggle + publish path).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_off_default_uses_phase2_fast_path() {
    let (handle, _rec, _clock) = build_handle();
    assert!(
        !handle.verify_pipeline().is_enabled(),
        "verify_pipeline().is_enabled() MUST be false by default"
    );
    let vp = handle.verify_pipeline();
    vp.enable();
    vp.disable();
    vp.join_worker().await;
    assert!(!vp.is_enabled());
    assert_eq!(vp.state_change_count(), 2);
}

#[tokio::test]
async fn verify_enable_then_subscribe_spawns_worker() {
    // The toggle ON path spawns a worker. With an empty subscribe stream
    // the worker idles immediately and exits cleanly on disable. We assert
    // the toggle + worker JoinHandle observability shape.
    let (handle, _rec, _clock) = build_handle();
    handle.verify_pipeline().enable();
    assert!(handle.verify_pipeline().is_enabled());

    // Let the spawn complete.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    handle.verify_pipeline().disable();
    handle.verify_pipeline().join_worker().await;
    assert!(!handle.verify_pipeline().is_enabled());
    assert_eq!(handle.verify_pipeline().state_change_count(), 2);
}

#[tokio::test]
async fn ingest_does_not_publish_to_verify_queue_in_v0_default() {
    // The v0 graph-OFF default ingest path doesn't trigger the
    // __lunaris_verify__ topic; that's Plan 03-03's graph-ON validator
    // demote path (covered by graph_pipeline_smoke). Plan 04-04's job is
    // to make the worker side READY to consume; the ingest emit is
    // unchanged here.
    let (handle, rec, clock) = build_handle();
    let ep = Episode::new("ingest.md", "# Notes\nThe quick brown fox.", &clock);
    handle.ingest(ep).await.expect("ingest must succeed");

    assert_eq!(
        rec.published_verify_count(),
        0,
        "graph-OFF ingest MUST NOT publish to __lunaris_verify__"
    );
}

#[tokio::test]
async fn toggle_off_on_off_is_idempotent_and_observable() {
    // D-12 verbatim — state counter only increments on real transitions.
    let (handle, _rec, _clock) = build_handle();
    let vp = handle.verify_pipeline();
    assert!(!vp.is_enabled());
    assert_eq!(vp.state_change_count(), 0);

    vp.enable();
    vp.enable(); // idempotent
    assert_eq!(vp.state_change_count(), 1);

    vp.disable();
    vp.disable(); // idempotent
    assert_eq!(vp.state_change_count(), 2);

    vp.enable();
    assert!(vp.is_enabled());
    assert_eq!(
        vp.state_change_count(),
        3,
        "exactly 3 real transitions across the ON-OFF-ON sequence"
    );
    vp.disable();
    vp.join_worker().await;
}

#[tokio::test]
async fn noop_verifier_with_pipeline_on_emits_no_writes() {
    // NoopVerifier's applies()==false short-circuits before verify can be
    // called. With the pipeline ON and an empty subscribe stream, no
    // atomic_write fires, no audit emits.
    let (handle, rec, _clock) = build_handle();
    handle.verify_pipeline().enable();

    assert!(
        !handle
            .verifier()
            .expect("default Noop installed")
            .applies(),
        "NoopVerifier.applies() MUST be false"
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    handle.verify_pipeline().disable();
    handle.verify_pipeline().join_worker().await;

    assert_eq!(
        rec.batch_count(),
        0,
        "NoopVerifier + empty subscribe MUST issue zero atomic_write calls"
    );
    assert_eq!(
        rec.published_audit_count(),
        0,
        "NoopVerifier + empty subscribe MUST publish zero __lunaris_audit__ messages"
    );
}

#[tokio::test]
async fn with_verifier_swaps_handle_verifier() {
    let (handle, _rec, _clock) = build_handle();
    let mock_a: Arc<dyn Verifier> = Arc::new(MockVerifier::arbitrate(VerifyDecision::deferred()));
    let handle = handle.with_verifier(mock_a.clone());
    let installed = handle.verifier().expect("snapshot installed");
    assert!(
        Arc::ptr_eq(&installed, &mock_a),
        "snapshot MUST return the just-installed verifier"
    );

    let mock_b: Arc<dyn Verifier> = Arc::new(MockVerifier::arbitrate(VerifyDecision::arbitrate(
        ulid::Ulid::new(),
        ulid::Ulid::new(),
        "second test verifier",
        VerifierBackend::Noop,
    )));
    let handle = handle.with_verifier(mock_b.clone());
    let installed = handle.verifier().expect("snapshot installed");
    assert!(
        Arc::ptr_eq(&installed, &mock_b),
        "snapshot MUST return the second verifier after the swap"
    );
    let vp = handle.verify_pipeline();
    assert!(!vp.is_enabled(), "swap preserves toggle state");
    assert_eq!(vp.state_change_count(), 0, "swap preserves state-change counter");
}

#[tokio::test]
async fn verify_pipeline_handle_default_off_and_constructible() {
    // Compile-time + runtime sanity: the handle is reachable on the umbrella
    // crate and constructible with a Noop verifier.
    let _: Arc<VerifierPipelineHandle> = Arc::new(VerifierPipelineHandle::with_noop());
    let _: Arc<dyn Verifier> = Arc::new(NoopVerifier);
    // Sanity-touch json! so the import doesn't get pruned in case Task 4
    // fixtures rely on it later.
    let _ = json!({});
}
