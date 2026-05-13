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
    /// Plan 04-04 Task 4: subscribe stream backing channel. The B-2
    /// integration test pushes one synthetic envelope here; the worker's
    /// subscribe stream pops it and process_one fires apply_supersede.
    /// Default: unbounded mpsc channel; a fresh receiver is taken on the
    /// first subscribe call (single-subscriber test fixture).
    subscribe_tx: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<QueueMsg>>>,
    subscribe_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<QueueMsg>>>,
}

impl Default for RecordingStorageWithKeyword {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<QueueMsg>();
        Self {
            rows: Mutex::new(HashMap::new()),
            chunk_ids: Mutex::new(Vec::new()),
            batches: Mutex::new(Vec::new()),
            published_messages: Mutex::new(Vec::new()),
            next_lsn: AtomicU64::new(1),
            subscribe_tx: tokio::sync::Mutex::new(Some(tx)),
            subscribe_rx: tokio::sync::Mutex::new(Some(rx)),
        }
    }
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }

    fn next_lsn_value(&self) -> u64 {
        self.next_lsn.fetch_add(1, Ordering::SeqCst).max(1)
    }

    /// Plan 04-04 Task 4 — push one envelope into the subscribe channel.
    /// The worker's subscribe stream surfaces it and process_one fires
    /// apply_supersede. Async because the tx slot is behind a tokio Mutex
    /// (the worker holds rx behind another tokio Mutex through its `.await`
    /// hot path; we keep the test mutexes async so neither blocks).
    async fn push_subscribe_envelope(&self, topic: &str, partition: u16, payload: Bytes) {
        let tx_guard = self.subscribe_tx.lock().await;
        if let Some(tx) = tx_guard.as_ref() {
            let _ = tx.send(QueueMsg { topic: topic.to_string(), partition, offset: 0, payload });
        }
    }

    fn published_verify_count(&self) -> usize {
        self.published_messages.lock().iter().filter(|(t, _, _)| t == "__lunaris_verify__").count()
    }

    fn published_audit_count(&self) -> usize {
        self.published_messages.lock().iter().filter(|(t, _, _)| t == "__lunaris_audit__").count()
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
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
        Ok(self.rows.lock().get(key).cloned().map(|r| Row {
            key: key.to_vec(),
            value: Bytes::from(r.value),
            bt: r.bt,
        }))
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
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
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        let mut rx_guard = self.subscribe_rx.lock().await;
        match rx_guard.take() {
            Some(rx) => {
                let stream =
                    stream::unfold(
                        rx,
                        |mut rx| async move { rx.recv().await.map(|msg| (Ok(msg), rx)) },
                    );
                Ok(Box::pin(stream))
            }
            None => Ok(Box::pin(stream::empty())),
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
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorageWithKeyword {
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
    async fn verify(&self, _item: VerifyNeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
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
    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "ingest.md",
        "# Notes\nThe quick brown fox.",
        &clock,
    );
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
        !handle.verifier().expect("default Noop installed").applies(),
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
    assert!(Arc::ptr_eq(&installed, &mock_a), "snapshot MUST return the just-installed verifier");

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

// ---------------------------------------------------------------------------
// Plan 04-04 Task 4 (B-2 + B-2-RESIDUAL) — real MVCC primitive-row supersede
// integration test.
// ---------------------------------------------------------------------------

/// Asserts the JSON-patch contract end-to-end:
///
/// 1. Seed two `fact:<ulid>` rows with payloads carrying `bt` as a
///    top-level JSON field.
/// 2. Push one VerifyEnvelope onto the subscribe channel referencing the
///    loser ulid.
/// 3. The MockVerifier returns `arbitrate(winner, loser, ...)` so
///    `apply_supersede` runs.
/// 4. After the worker drains, assert against the PERSISTED bytes — NOT
///    against `row.bt` — because the contract is "the bt mutation rides
///    inside the WriteOp::KvPut value bytes".
///
/// B-2-RESIDUAL guards: the test parses the persisted payload as
/// `serde_json::Value` and asserts `payload["bt"]["sys"][1]` is non-null
/// for the loser AND `payload["bt"]["sys"][1]` is null for the winner.
#[tokio::test]
async fn verify_apply_supersede_writes_real_mvcc_rows() {
    use lunaris_extract::types::Fact;

    let (handle, rec, _clock) = build_handle();
    let loser_ulid = ulid::Ulid::new();
    let winner_ulid = ulid::Ulid::new();
    let alice_id = lunaris_extract::EntityId::from_name_and_type("Alice", "Person");
    let bob_id = lunaris_extract::EntityId::from_name_and_type("Bob", "Person");
    let loser_key = format!("fact:{loser_ulid}").into_bytes();
    let winner_key = format!("fact:{winner_ulid}").into_bytes();
    let t0 = Hlc::ZERO;

    // Build payloads that are valid `lunaris_extract::types::Fact` JSON
    // PLUS carry a top-level `bt` field. The Fact struct doesn't have a
    // `bt` field of its own, so we serialize it then merge in the bt
    // value — the JSON object is what matters for the apply_supersede
    // JSON-patch contract.
    let loser_fact = Fact {
        id: loser_ulid,
        subject_id: alice_id,
        predicate: "knows".into(),
        object_id: bob_id,
        fact_text: "Alice knows Bob (loser)".into(),
        confidence: 0.5,
        valid_from_iso: "2024-01-01T00:00:00Z".into(),
        valid_to_iso: None,
    };
    let winner_fact = Fact {
        id: winner_ulid,
        subject_id: alice_id,
        predicate: "knows".into(),
        object_id: bob_id,
        fact_text: "Alice knows Bob (winner)".into(),
        confidence: 0.95,
        valid_from_iso: "2024-01-01T00:00:00Z".into(),
        valid_to_iso: None,
    };
    let loser_payload = {
        let mut v = serde_json::to_value(&loser_fact).unwrap();
        v["bt"] = serde_json::to_value(BiTemporal::at(t0, t0)).unwrap();
        serde_json::to_vec(&v).unwrap()
    };
    let winner_payload = {
        let mut v = serde_json::to_value(&winner_fact).unwrap();
        v["bt"] = serde_json::to_value(BiTemporal::at(t0, t0)).unwrap();
        serde_json::to_vec(&v).unwrap()
    };

    rec.rows.lock().insert(
        loser_key.clone(),
        MockRow { value: loser_payload.clone(), bt: BiTemporal::at(t0, t0) },
    );
    rec.rows.lock().insert(
        winner_key.clone(),
        MockRow { value: winner_payload.clone(), bt: BiTemporal::at(t0, t0) },
    );

    // Capture batch_count BEFORE we spawn — we want to assert that exactly
    // ONE atomic_write happens for the supersede pair.
    let batch_count_before = rec.batch_count();

    // Wire MockVerifier returning arbitrate(winner, loser, ...).
    let decision = VerifyDecision::arbitrate(
        winner_ulid,
        loser_ulid,
        "winner has higher confidence",
        VerifierBackend::Noop,
    );
    let verifier: Arc<dyn Verifier> = Arc::new(MockVerifier::arbitrate(decision));
    let handle = handle.with_verifier(verifier);

    // Push the synthetic verify envelope onto the subscribe channel BEFORE
    // enabling the pipeline (the worker's subscribe stream is taken on
    // first .subscribe() call inside spawn_worker_if_idle). The reason is
    // shaped as InvalidBitemporal — easiest variant to construct without
    // synthesising a contradiction graph; the apply_supersede path doesn't
    // care about the reason, only the kind + winner/loser ids.
    let envelope = json!({
        "kind": "fact",
        "item": {
            "reason": {
                "InvalidBitemporal": {
                    "valid_from": "2026-01-01T00:00:00Z",
                    "valid_to": "2025-01-01T00:00:00Z"
                }
            },
            "raw": loser_fact
        }
    });
    let payload = Bytes::from(serde_json::to_vec(&envelope).unwrap());
    rec.push_subscribe_envelope("__lunaris_verify__", 0, payload).await;

    // Spawn the worker.
    handle.verify_pipeline().enable();
    assert!(handle.verify_pipeline().is_enabled());

    // Give the worker a moment to consume + apply.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Shutdown the worker cleanly.
    handle.verify_pipeline().disable();
    handle.verify_pipeline().join_worker().await;

    // -- Assertions --

    // 1. Exactly ONE atomic_write happened for the supersede pair (D-11).
    let batch_count_after = rec.batch_count();
    assert_eq!(
        batch_count_after - batch_count_before,
        1,
        "D-11 invariant: exactly ONE atomic_write per VerifyDecision (got {} new batches)",
        batch_count_after - batch_count_before
    );

    // 2. The single batch carries exactly TWO ops (loser + winner).
    let last_batch = rec.batches.lock().last().cloned().expect("batch present");
    assert_eq!(
        last_batch.len(),
        2,
        "D-11 invariant: ONE atomic_write, TWO WriteOps (loser + winner); got {}",
        last_batch.len()
    );

    // 3. Read back the persisted LOSER payload and assert payload["bt"]["sys"][1]
    //    is non-null. This is the B-2-RESIDUAL guard: the test inspects the raw
    //    persisted bytes — NOT row.bt — because the contract is that the bt
    //    mutation rides inside the WriteOp::KvPut value bytes.
    let rows = rec.rows.lock();
    let loser_row = rows.get(&loser_key).expect("loser row still present");
    let loser_payload: serde_json::Value =
        serde_json::from_slice(&loser_row.value).expect("loser payload is valid JSON");
    let loser_sys_1 = &loser_payload["bt"]["sys"][1];
    assert!(
        !loser_sys_1.is_null(),
        "B-2-RESIDUAL: loser payload bt.sys[1] MUST be Some(now); got null. \
         JSON patch was NOT applied to the WriteOp::KvPut value bytes. \
         Persisted payload: {loser_payload}"
    );

    // 4. WINNER assertion: bt.sys[0] is the fresh `now` (non-zero) AND
    //    bt.sys[1] is null (open validity).
    let winner_row = rows.get(&winner_key).expect("winner row still present");
    let winner_payload: serde_json::Value =
        serde_json::from_slice(&winner_row.value).expect("winner payload is valid JSON");
    let winner_sys_0 = &winner_payload["bt"]["sys"][0];
    let winner_sys_1 = &winner_payload["bt"]["sys"][1];
    let t0_value = serde_json::to_value(t0).unwrap();
    assert!(
        !winner_sys_0.is_null() && winner_sys_0 != &t0_value,
        "B-2-RESIDUAL: winner payload bt.sys[0] MUST be the fresh `now`, not t0; \
         got {winner_sys_0}"
    );
    assert!(
        winner_sys_1.is_null(),
        "B-2-RESIDUAL: winner payload bt.sys[1] MUST be null (open validity); got {winner_sys_1}"
    );

    // 5. The audit publish DID fire (Plan 04-01 D-22).
    assert!(
        rec.published_audit_count() >= 1,
        "expected at least 1 __lunaris_audit__ publish for the arbitration; got {}",
        rec.published_audit_count()
    );
}
