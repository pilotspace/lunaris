// Tests exercise the deprecated `Lunaris::forget` path on purpose — the
// canonical entry point is `ScopedLunaris::forget(request)` (RFC 0001), but
// this module retains coverage of the deprecated wrapper until it is removed.
#![allow(deprecated)]

//! Plan 04-05 — integration smoke tests for `Lunaris::forget(target)`.
//!
//! Seven mandatory test cases per PATTERNS.md `forget_smoke.rs` section:
//!
//! 1. `forget_id_writes_one_mvcc_row_and_publishes_audit` (OPS-01)
//! 2. `forget_scope_by_source_prefix_soft_deletes_all_matches` (OPS-02 + D-20)
//! 3. `forget_before_temporal_purge` (OPS-03 + W-1 typed Hlc compare)
//! 4. `forget_dry_run_writes_zero_rows_returns_preview` (D-21 step 1)
//! 5. `forget_hard_without_confirmation_token_fails` (B-3 typed
//!    `ValidateError::ConfirmationRequired`)
//! 6. `forget_hard_with_confirmation_token_issues_irreversible_delete`
//!    (D-21 full two-step protocol)
//! 7. `forget_publishes_audit_event_on_every_call` (OPS-04 + D-22)
//!
//! ## B-6 fix — actual StoragePort signatures
//!
//! The mock `RecordingStorageWithKeyword` impls every trait method with the
//! ACTUAL signature from `crates/lunaris-core/src/storage/port.rs`:
//!
//! - `vector_search(&str, &[f32], usize, Option<&Filter>, Option<Hlc>, bool)`
//!   (6 params, NOT 4)
//! - `graph_traverse(&CypherQuery, Option<Hlc>)` (refs, NOT owned)
//! - `Row { key, value, bt }` (3 fields including `key`)
//! - `Hlc::ZERO` const (Hlc has NO Default impl)
//! - `BiTemporal { valid: (Hlc, Option<Hlc>), sys: (Hlc, Option<Hlc>) }`
//!   tuple shape (NO Default impl; explicit init)
//! - `HlcClock::new(0)` returns `Arc<HlcClock>` directly (do NOT wrap with
//!   `Arc::new(HlcClock::new(0))` — that would double-Arc)

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::{AuditEvent, ForgetTarget, IndexKind, Lunaris, ScopeSpec};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder, ValidateError,
};
use parking_lot::Mutex;
use serde_json::json;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Mock fixture — RecordingStorageWithKeyword (B-6 actual signatures)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingStorageWithKeyword {
    /// Per-key (payload, bt) tuple. `bt` is stored separately so `read_as_of`
    /// can return the typed `Row { key, value, bt }` without re-deriving
    /// from the payload bytes.
    #[allow(clippy::type_complexity)]
    rows: Mutex<HashMap<Vec<u8>, (Vec<u8>, BiTemporal)>>,
    /// Full atomic_write history — `len()` == number of atomic_write calls
    /// (D-19 single-call invariant test target).
    batches: Mutex<Vec<Vec<WriteOp>>>,
    /// Topic + partition + payload tuple for every `publish` call. Filtered
    /// by topic in `published_audit_events()` for the audit-emit assertions.
    published_messages: Mutex<Vec<(String, u16, Bytes)>>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().len()
    }

    /// Decode every `__lunaris_audit__` payload into the typed `AuditEvent`
    /// enum. Filters out other-topic publishes (verify-queue,
    /// consolidate-queue) so OPS-04 assertions stay precise.
    fn published_audit_events(&self) -> Vec<AuditEvent> {
        self.published_messages
            .lock()
            .iter()
            .filter(|(t, _, _)| t == "__lunaris_audit__")
            .filter_map(|(_, _, p)| serde_json::from_slice::<AuditEvent>(p).ok())
            .collect()
    }

    /// Seed an episode row with the given source + bt valid_from. The
    /// in-memory payload mirrors the on-the-wire shape Lunaris writes:
    /// `{ id, source, content, bt: { valid: [hlc, null], sys: [hlc, null] } }`.
    fn seed_episode(&self, key: &str, source: &str, valid_from: Hlc) {
        let payload = serde_json::to_vec(&json!({
            "id": Ulid::new().to_string(),
            "source": source,
            "content": "test",
            "bt": {
                "valid": [valid_from, null],
                "sys":   [valid_from, null],
            },
        }))
        .unwrap();
        // B-6: explicit BiTemporal init — Hlc has Hlc::ZERO; BiTemporal has no Default.
        let bt = BiTemporal { valid: (valid_from, None), sys: (valid_from, None) };
        self.rows.lock().insert(key.as_bytes().to_vec(), (payload, bt));
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    let prior_bt =
                        self.rows.lock().get(key).map(|(_, bt)| *bt).unwrap_or(BiTemporal {
                            valid: (Hlc::ZERO, None),
                            sys: (Hlc::ZERO, None),
                        });
                    self.rows.lock().insert(key.clone(), (value.clone(), prior_bt));
                }
                WriteOp::KvDelete { key } => {
                    self.rows.lock().remove(key);
                }
                _ => {}
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
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        let snapshot = self.rows.lock().clone();
        let pairs: Vec<Result<(Bytes, Bytes), StorageError>> = snapshot
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, (v, _bt))| Ok((Bytes::from(k), Bytes::from(v))))
            .collect();
        Ok(Box::pin(stream::iter(pairs)))
    }

    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        let rows = self.rows.lock();
        Ok(rows.get(key).map(|(v, bt)| Row {
            key: key.to_vec(),
            value: Bytes::from(v.clone()),
            bt: *bt,
        }))
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.published_messages.lock().push((topic.to_string(), partition, payload));
        Ok(self.published_messages.lock().len() as u64)
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: false,
            rerank_native: false,
            queue_native: true,
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

/// Construct a Lunaris handle backed by the recording fixture. `clock` and
/// `embedder` are constructed inline because Lunaris::with_parts_keyword
/// takes `Arc<HlcClock>` directly (B-6: `HlcClock::new(0)` already returns
/// `Arc<HlcClock>`; do NOT double-wrap).
fn build_lunaris(rec: Arc<RecordingStorageWithKeyword>) -> Lunaris {
    Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        // B-6: HlcClock::new returns Arc<HlcClock> directly. Wrapping with
        // Arc::new(...) again would yield Arc<Arc<HlcClock>>.
        HlcClock::new(0),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_id_writes_one_mvcc_row_and_publishes_audit() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let target_ulid = Ulid::new();
    rec.seed_episode(&format!("episode:{target_ulid}"), "test:source", Hlc::ZERO);

    let lunaris = build_lunaris(rec.clone());
    let receipt = lunaris.forget(ForgetTarget::Id(target_ulid)).await.expect("forget id");

    assert_eq!(receipt.rows_written, 1, "one MVCC sys_to write per matched row");
    assert_eq!(receipt.rows_deleted, 0, "soft-delete writes ZERO hard deletes");
    assert!(!receipt.preview, "non-dry_run receipt has preview=false");
    assert_eq!(
        rec.batch_count(),
        1,
        "D-19 single-call invariant: ONE atomic_write per forget call"
    );
    assert!(receipt.indices_affected.contains(&IndexKind::Kv), "indices_affected reports KV");

    let audits = rec.published_audit_events();
    assert_eq!(audits.len(), 1, "OPS-04: every forget call publishes ONE __lunaris_audit__ event");
    assert!(matches!(audits[0], AuditEvent::Forget(_)));
}

#[tokio::test]
async fn forget_scope_by_source_prefix_soft_deletes_all_matches() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    for i in 0..3 {
        rec.seed_episode(&format!("episode:helios-{i}"), "helios:fs/session-42/foo.md", Hlc::ZERO);
    }
    for i in 0..2 {
        rec.seed_episode(&format!("episode:other-{i}"), "other:source", Hlc::ZERO);
    }

    let lunaris = build_lunaris(rec.clone());
    let receipt = lunaris
        .forget(ForgetTarget::Scope(ScopeSpec::BySource("helios:fs/session-42/".into())))
        .await
        .expect("forget scope");

    assert_eq!(receipt.rows_written, 3, "3 helios-prefix episodes match D-20 BySource");
    assert_eq!(rec.batch_count(), 1, "D-19 single-call invariant: ONE atomic_write batch");

    // B-5 contract: the persisted payload for at least one matched row must
    // have `bt.sys[1]` patched non-null after the soft delete.
    let rows = rec.rows.lock().clone();
    let matched_row = rows
        .get(b"episode:helios-0".as_slice())
        .expect("seeded row still present (soft-delete preserves)");
    let (payload, _) = matched_row;
    let json: serde_json::Value = serde_json::from_slice(payload).expect("parse payload");
    let sys_to = json
        .get("bt")
        .and_then(|bt| bt.get("sys"))
        .and_then(|sys| sys.get(1))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    assert!(
        !sys_to.is_null(),
        "B-5: soft-delete MUST patch payload['bt']['sys'][1] non-null; got payload={json}"
    );
}

#[tokio::test]
async fn forget_before_temporal_purge() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let t1 = Hlc { wall_ms: 100, counter: 0, node_id: 0 };
    let t3 = Hlc { wall_ms: 300, counter: 0, node_id: 0 };
    rec.seed_episode("episode:t1", "test:s", t1);
    rec.seed_episode("episode:t3", "test:s", t3);

    let lunaris = build_lunaris(rec.clone());
    let cutoff = Hlc { wall_ms: 200, counter: 0, node_id: 0 };
    let receipt = lunaris.forget(ForgetTarget::Before(cutoff)).await.expect("forget before");

    // W-1 typed compare: only t1 < cutoff (100 < 200 → true; 300 < 200 → false).
    assert_eq!(
        receipt.rows_written, 1,
        "exactly 1 row matches Before(t=200) with W-1 typed Hlc Ord compare"
    );
    assert_eq!(rec.batch_count(), 1, "D-19 single-call invariant");
}

#[tokio::test]
async fn forget_dry_run_writes_zero_rows_returns_preview() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let target_ulid = Ulid::new();
    rec.seed_episode(&format!("episode:{target_ulid}"), "test:source", Hlc::ZERO);

    let lunaris = build_lunaris(rec.clone());
    let receipt =
        lunaris.forget(ForgetTarget::Id(target_ulid).dry_run()).await.expect("dry_run forget");

    assert!(receipt.preview, "dry_run receipt MUST carry preview=true");
    assert_eq!(receipt.rows_written, 0);
    assert_eq!(receipt.rows_deleted, 0);
    assert_eq!(
        rec.batch_count(),
        0,
        "dry_run MUST NOT call atomic_write (D-21 safety rail step 1)"
    );
    // Audit fires even on dry_run — the forget call ITSELF is the audited
    // event (the caller asked the question).
    assert_eq!(rec.published_audit_events().len(), 1);
}

/// B-3 typed-variant assertion: hard-delete without prior dry_run +
/// confirm_hard_forget MUST return
/// `Err(LunarisError::Validate(ValidateError::ConfirmationRequired(_)))`.
/// NOT a string-shape Validate error.
#[tokio::test]
async fn forget_hard_without_confirmation_token_fails() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let lunaris = build_lunaris(rec.clone());
    let result = lunaris.forget(ForgetTarget::Id(Ulid::new()).hard()).await;
    match result {
        Err(LunarisError::Validate(ValidateError::ConfirmationRequired(msg))) => {
            assert!(
                msg.contains("confirmation"),
                "ConfirmationRequired message MUST mention 'confirmation'; got: {msg}"
            );
        }
        other => panic!("expected ValidateError::ConfirmationRequired; got {other:?}"),
    }
    assert_eq!(rec.batch_count(), 0, "no atomic_write must fire on the failed validation path");
    assert_eq!(
        rec.published_audit_events().len(),
        0,
        "no audit must fire on the failed validation path"
    );
}

#[tokio::test]
async fn forget_hard_with_confirmation_token_issues_irreversible_delete() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let target_ulid = Ulid::new();
    let key = format!("episode:{target_ulid}");
    rec.seed_episode(&key, "test:source", Hlc::ZERO);

    let lunaris = build_lunaris(rec.clone());

    // Step 1: dry_run to receive a preview receipt.
    let dry_receipt =
        lunaris.forget(ForgetTarget::Id(target_ulid).dry_run()).await.expect("dry_run");
    assert!(dry_receipt.preview);

    // Step 2: confirm_hard_forget(receipt) → confirmation token.
    let token = lunaris.confirm_hard_forget(dry_receipt).await.expect("confirm");

    // Step 3: hard().with_token(token) → irreversible delete via KvDelete.
    let receipt = lunaris
        .forget(ForgetTarget::Id(target_ulid).hard().with_token(token))
        .await
        .expect("hard delete");
    assert_eq!(receipt.rows_deleted, 1, "hard delete reports ONE deleted row");
    assert_eq!(receipt.rows_written, 0);
    assert!(
        !rec.rows.lock().contains_key(key.as_bytes()),
        "hard delete MUST physically remove the key from the backing store"
    );
}

#[tokio::test]
async fn forget_publishes_audit_event_on_every_call() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    rec.seed_episode("episode:e1", "test:source", Hlc::ZERO);

    let lunaris = build_lunaris(rec.clone());
    let _ = lunaris.forget(ForgetTarget::Id(Ulid::new())).await;
    let _ = lunaris.forget(ForgetTarget::Scope(ScopeSpec::BySource("test:".into()))).await;

    assert_eq!(
        rec.published_audit_events().len(),
        2,
        "OPS-04: ONE audit publish per forget call regardless of match count"
    );
    for event in rec.published_audit_events() {
        match event {
            AuditEvent::Forget(_) => {}
            other => panic!("expected AuditEvent::Forget, got {other:?}"),
        }
    }
}
