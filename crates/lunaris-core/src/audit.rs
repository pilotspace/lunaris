//! Phase 13 Plan 13-01 — canonical `AuditEvent` for the `__lunaris_audit__`
//! unified audit topic. Moved from `crates/lunaris/src/audit.rs` to
//! `lunaris-core::audit` per D1 + D2 so the type stays leaf-pure (no
//! worker-crate dependency) and every publisher converges on a single JSON
//! wire shape.
//!
//! ## Wire-shape contract (frozen at v0.1.0)
//!
//! The JSON produced by `serde_json::to_vec(&event)` MUST be byte-identical to
//! the four committed fixtures at
//! `crates/lunaris-core/tests/fixtures/audit/v0.1.0/{forget,verifier_arbitration,consolidator_promotion,consolidator_archive}.json`.
//! The fixture-parity integration test enforces this — any schema drift
//! breaks the test.
//!
//! ## Leaf-purity
//!
//! This module does NOT depend on `lunaris-verify`, `lunaris-consolidate`, or
//! `crates/lunaris`. Variants carry nested-mirror types (`ForgetReceiptData`,
//! `ForgetTargetData`, `ScopeSpecData`, `IndexKindData`, `FactIdData`) that
//! re-declare the shape locally. Callers in worker crates convert via
//! trivial `From` impls or struct-literal construction.
//!
//! ## Publisher abstraction
//!
//! [`Publisher`] is a narrow trait that generalizes the `StoragePort::publish`
//! contract. A blanket impl is provided for `Arc<dyn StoragePort>` so every
//! existing caller can keep passing `&storage` unchanged. The
//! [`publish_audit_event`] helper serializes + publishes with the same
//! fire-and-forget semantics as the pre-refactor version in
//! `crates/lunaris/src/audit.rs` (`tracing::warn!` on failure; never
//! propagates).
//!
//! ## v0.1.0 Info #1 closure (RELEASE-01 + RELEASE-02)
//!
//! Before this plan, both worker crates constructed inline
//! `serde_json::json!({ "kind": "...", ... })` envelopes that drifted from
//! the typed enum (notably: workers stringified `fact_id` while the enum
//! serializes as a byte array). This module is the single point of edit for
//! the audit shape going forward; CI grep gate (Plan 13-01 Task 3) blocks
//! reintroduction of the inline pattern.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::scope::Scope;
use crate::storage::types::Lsn;
use crate::{StorageError, StoragePort};

/// Unified audit topic. Every publisher in the workspace emits here.
pub const AUDIT_TOPIC: &str = "__lunaris_audit__";

// ---------------------------------------------------------------------------
// Nested-mirror types (byte-identical to the v0.1.0 shapes declared in
// `crates/lunaris/src/forget.rs` + `crates/lunaris-consolidate/src/types.rs`).
// Kept as local declarations so `lunaris-core::audit` stays a leaf.
// ---------------------------------------------------------------------------

/// Mirror of `lunaris_consolidate::FactId` — `[u8; 16]` newtype. Serializes as
/// a JSON array of 16 numbers (default serde shape for `[u8; 16]`). Workers
/// construct via `FactIdData(fact_id.0)` or an explicit `From` impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactIdData(pub [u8; 16]);

/// Mirror of `lunaris::forget::IndexKind`. External-tag serialization ⇒ bare
/// strings `"Kv"` / `"Vector"` / `"Graph"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum IndexKindData {
    Kv,
    Vector,
    Graph,
}

/// Mirror of `lunaris::forget::ScopeSpec`. External-tag serialization ⇒
/// `{"BySource":"..."}` / `{"ByMetadata":["k","v"]}` / `{"ByEpisode":"<ulid>"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ScopeSpecData {
    BySource(String),
    ByMetadata(String, String),
    ByEpisode(Ulid),
}

/// Mirror of `lunaris::forget::ForgetTarget`. External-tag serialization ⇒
/// `{"Id":"<ulid>"}` / `{"Scope":{...}}` / `{"Before":{<hlc>}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ForgetTargetData {
    Id(Ulid),
    Scope(ScopeSpecData),
    Before(crate::Hlc),
}

/// Mirror of `lunaris::forget::ForgetReceipt`. Field names and nesting match
/// the v0.1.0 wire shape exactly (validated by `forget.json` fixture).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetReceiptData {
    pub target: ForgetTargetData,
    pub indices_affected: Vec<IndexKindData>,
    pub rows_written: u64,
    pub rows_deleted: u64,
    pub audit_lsn: Lsn,
    pub preview: bool,
}

// ---------------------------------------------------------------------------
// Canonical AuditEvent
// ---------------------------------------------------------------------------

/// Canonical typed audit event. Externally-tagged so the JSON wire shape
/// carries `"kind": "<variant-name>"` for grep-friendly ops triage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum AuditEvent {
    /// Emitted by `Lunaris::forget` after every successful call (D-22 / OPS-04).
    Forget(ForgetReceiptData),

    /// Emitted by the lunaris-verify worker after every applied
    /// `VerifyDecision`. `backend` is a bare string — the upstream
    /// `VerifierBackend` enum serializes as external tag (`"CloudAnthropic"`);
    /// the flat `String` on this canonical variant preserves that byte shape
    /// without dragging `lunaris-verify` into the core dependency graph.
    VerifierArbitration {
        winner_id: Option<String>,
        loser_id: Option<String>,
        reason: String,
        backend: String,
        decided_at_iso: String,
    },

    /// Per-promotion event (one per Episode→Fact promotion).
    ConsolidatorPromotion { episode_id: Ulid, fact_id: FactIdData, activation_score: f64 },

    /// Per-archive event (one per archived Fact).
    ConsolidatorArchive { fact_id: FactIdData, final_activation: f64, moved_to: String },

    /// Emitted by `ScopedLunaris::end_turn` after each successful reflect-driven
    /// MVCC invalidation (D-22). One event per fact ulid stamped. `turn_id` is
    /// `None` when the caller did not supply a turn boundary identifier.
    ReflectInvalidation {
        /// The fact ulid whose `bt.sys.1` was stamped.
        ulid: String,
        /// The tenant scope under which the invalidation was applied.
        scope: String,
        /// RFC-3339 timestamp of the invalidation (wall-clock, not HLC).
        invalidated_at_iso: String,
        /// Optional turn identifier supplied by the caller via `ReflectInput::turn_id`.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Publisher trait + blanket impl for Arc<dyn StoragePort>
// ---------------------------------------------------------------------------

/// Publish error returned by [`publish_audit_event`]. Always logged via
/// `tracing::warn!` — callers may ignore the error per the fire-and-forget
/// contract (blueprint §11).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    #[error("audit serialize failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("audit publish backend failed: {0}")]
    Backend(String),
}

/// Narrow publish surface. Decouples [`publish_audit_event`] from
/// [`StoragePort`] so tests can substitute an in-memory capture without
/// implementing the full `StoragePort` trait.
///
/// W4.6: `publish` carries the producing `scope`. It did not until 0.7.0, and
/// the omission was not cosmetic — Moon namespaces MQ topics per scope
/// (`lunaris:{scope}:{topic}`), so every audit event in the workspace landed
/// on `Scope::dev()`'s topic regardless of who produced it. A tenant reading
/// their own audit stream — the only stream they are entitled to read — saw
/// nothing for operations that definitely happened, and every tenant's
/// receipts piled into one shared partition.
#[async_trait]
pub trait Publisher: Send + Sync {
    async fn publish(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, PublishError>;
}

#[async_trait]
impl Publisher for Arc<dyn StoragePort> {
    async fn publish(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, PublishError> {
        StoragePort::publish(self.as_ref(), scope, topic, partition, payload)
            .await
            .map_err(|e: StorageError| PublishError::Backend(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// publish_audit_event helper (fire-and-forget; tracing::warn! on failure)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Audit consumer (W4.6 / D6.3 — G2)
// ---------------------------------------------------------------------------

/// One decoded audit record, with the broker offset it was read at.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AuditRecord {
    /// Broker-assigned offset, monotonically increasing within a topic.
    pub offset: u64,
    pub event: AuditEvent,
}

/// The result of one [`read_audit_events`] call.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct AuditPage {
    /// Decoded records, oldest first.
    pub records: Vec<AuditRecord>,
    /// Entries that were in range but whose payload did not decode as an
    /// [`AuditEvent`].
    ///
    /// Surfaced rather than silently skipped: an audit reader that quietly
    /// drops what it cannot parse reports "no records" for a topic that has
    /// them, which is the same failure the drop counter exists to prevent. A
    /// non-zero value means the topic holds entries this build cannot read —
    /// a foreign producer, or a format older than the current `AuditEvent`.
    pub undecodable: usize,
}

/// Read a scope's audit trail over a closed time range. **Non-destructive** —
/// see [`crate::StoragePort::queue_range`].
///
/// This is the consumer the D6 decision's G2 named as missing: before it,
/// `grep` for a reader against [`AUDIT_TOPIC`] returned producers only, and a
/// write-only audit log answers no governance question. "Who deleted this?" is
/// exactly the query the trail exists to serve.
///
/// `from_ms` / `to_ms` are inclusive wall-clock milliseconds; `None` is
/// unbounded on that side. Records come back oldest-first, capped at `limit`.
///
/// Reads only `scope`'s own topic. Since W4.6 that is also the only place
/// `scope`'s events are written, so this cannot serve one tenant another
/// tenant's history — the property that made the scope threading a hard
/// prerequisite rather than a cleanup.
pub async fn read_audit_events(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    limit: usize,
) -> Result<AuditPage, StorageError> {
    let msgs = storage.queue_range(scope, AUDIT_TOPIC, 0, from_ms, to_ms, limit).await?;
    let mut page = AuditPage { records: Vec::with_capacity(msgs.len()), undecodable: 0 };
    for msg in msgs {
        match serde_json::from_slice::<AuditEvent>(&msg.payload) {
            Ok(event) => page.records.push(AuditRecord { offset: msg.offset, event }),
            Err(e) => {
                tracing::warn!(
                    offset = msg.offset,
                    err = %e,
                    "audit entry did not decode as an AuditEvent; counted as undecodable"
                );
                page.undecodable += 1;
            }
        }
    }
    Ok(page)
}

// ---------------------------------------------------------------------------
// Dropped-event counter (W4.6 / D6.3 — G3)
// ---------------------------------------------------------------------------

/// Process-wide count of audit events that were produced but never reached
/// the broker.
///
/// `publish_audit_event` is fire-and-forget by blueprint §11: a broker hiccup
/// must not roll back a user's committed `forget`. That is the right call, and
/// it is also why "we have no record" and "it did not happen" were the same
/// observable state — the D6 decision's G3. Fire-and-forget stays; the drop is
/// now COUNTABLE, so an operator sees a gap rather than inferring one.
///
/// Deliberately a plain atomic in `lunaris-core` rather than a `prometheus`
/// metric: core carries no metrics dependency, and the counter must increment
/// on every surface — MCP, hook, CLI, HTTP — not only the one that happens to
/// serve `/metrics`. `lunaris-server` mirrors it into
/// `lunaris_audit_events_dropped_total` at scrape time.
static AUDIT_EVENTS_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Read the process-wide dropped-audit-event count. Monotonic for the life of
/// the process; never reset.
pub fn audit_events_dropped() -> u64 {
    AUDIT_EVENTS_DROPPED.load(Ordering::Relaxed)
}

/// Fire-and-forget audit publish. Mirrors the pre-refactor helper at
/// `crates/lunaris/src/audit.rs:99-117` verbatim:
///
/// 1. Try to serialize the event to JSON bytes.
/// 2. On serialize failure: bump [`audit_events_dropped`], `tracing::warn!`,
///    return `Ok(0)`.
/// 3. On publish failure: bump [`audit_events_dropped`], `tracing::warn!`,
///    return `Ok(0)` — **never** propagate. The caller's mutation already
///    committed via `atomic_write`; an audit-channel hiccup must not roll back
///    the user's write.
/// 4. On success: returns the broker-assigned offset.
///
/// Returns `Ok(u64)` on both success AND soft-failure so existing callers
/// that bind the offset (e.g. `ForgetReceipt::audit_lsn`) keep working.
/// The `PublishError` variant is reserved for future strict-mode callers.
pub async fn publish_audit_event<P: Publisher + ?Sized>(
    publisher: &P,
    scope: &Scope,
    event: AuditEvent,
) -> Result<u64, PublishError> {
    let payload = match serde_json::to_vec(&event) {
        Ok(b) => b,
        Err(e) => {
            AUDIT_EVENTS_DROPPED.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(err = %e, "audit serialize failed; skipping audit publish");
            return Ok(0);
        }
    };
    match publisher.publish(scope, AUDIT_TOPIC, 0, payload.into()).await {
        Ok(offset) => Ok(offset),
        Err(e) => {
            AUDIT_EVENTS_DROPPED.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                err = %e,
                "audit publish failed; caller mutation still succeeded"
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// In-memory test publisher that captures every payload.
    struct CapturePublisher {
        pub inbox: Mutex<Vec<(String, String, u16, Bytes)>>,
    }
    impl CapturePublisher {
        fn new() -> Self {
            Self { inbox: Mutex::new(Vec::new()) }
        }
    }
    #[async_trait]
    impl Publisher for CapturePublisher {
        async fn publish(
            &self,
            scope: &Scope,
            topic: &str,
            partition: u16,
            payload: Bytes,
        ) -> Result<u64, PublishError> {
            let mut box_ = self.inbox.lock();
            box_.push((scope.as_str().to_string(), topic.to_string(), partition, payload));
            Ok(box_.len() as u64)
        }
    }

    #[tokio::test]
    async fn publish_audit_event_forget_round_trip() {
        let pub_ = CapturePublisher::new();
        let event = AuditEvent::Forget(ForgetReceiptData {
            target: ForgetTargetData::Scope(ScopeSpecData::BySource("x".into())),
            indices_affected: vec![IndexKindData::Kv],
            rows_written: 1,
            rows_deleted: 0,
            audit_lsn: Lsn { wall_ms: 1, counter: 0 },
            preview: false,
        });
        let scope = Scope::new("tenant-a").unwrap();
        let off = publish_audit_event(&pub_, &scope, event.clone()).await.unwrap();
        assert_eq!(off, 1);
        let inbox = pub_.inbox.lock();
        // W4.6: the scope the caller supplied reaches the publisher verbatim.
        // It used to be dropped on the floor here and replaced with
        // `Scope::dev()` inside the `Arc<dyn StoragePort>` impl, which on a
        // scope-namespaced broker filed every tenant's receipts in one shared
        // partition and left each tenant's own audit stream empty.
        assert_eq!(inbox[0].0, "tenant-a");
        assert_eq!(inbox[0].1, AUDIT_TOPIC);
        let decoded: AuditEvent = serde_json::from_slice(&inbox[0].3).unwrap();
        assert_eq!(decoded, event);
    }

    /// A publisher that always fails, so the soft-failure branch is reachable.
    struct FailingPublisher;
    #[async_trait]
    impl Publisher for FailingPublisher {
        async fn publish(
            &self,
            _scope: &Scope,
            _topic: &str,
            _partition: u16,
            _payload: Bytes,
        ) -> Result<u64, PublishError> {
            Err(PublishError::Backend("broker down".into()))
        }
    }

    /// W4.6 / D6.3 — G3: a dropped audit event must be COUNTABLE.
    ///
    /// Fire-and-forget stays — the assertion below pins that a broker failure
    /// still returns `Ok(0)` and never propagates, because the caller's
    /// mutation has already committed. What changes is that the loss stops
    /// being invisible: "we have no record" and "it did not happen" were the
    /// same observable state before this counter existed.
    ///
    /// Both cases live in ONE test on purpose. The counter is process-global
    /// and this module's tests run in parallel, so a second test touching it
    /// would make both flaky. Deltas rather than absolutes for the same
    /// reason.
    #[tokio::test]
    async fn a_dropped_audit_event_is_counted_and_a_delivered_one_is_not() {
        let event = AuditEvent::Forget(ForgetReceiptData {
            target: ForgetTargetData::Scope(ScopeSpecData::BySource("x".into())),
            indices_affected: vec![IndexKindData::Kv],
            rows_written: 1,
            rows_deleted: 0,
            audit_lsn: Lsn { wall_ms: 1, counter: 0 },
            preview: false,
        });
        let scope = Scope::new("tenant-a").unwrap();

        let before = audit_events_dropped();
        let off = publish_audit_event(&FailingPublisher, &scope, event.clone())
            .await
            .expect("a broker failure must NOT propagate — the caller's write already committed");
        assert_eq!(off, 0, "a dropped event has no broker offset");
        assert_eq!(
            audit_events_dropped(),
            before + 1,
            "a publish that never reached the broker was not counted, so the gap is invisible \
             to an operator — which is exactly the G3 defect this counter closes"
        );

        // And the counter must not fire on the happy path, or it measures
        // traffic instead of loss.
        let ok_pub = CapturePublisher::new();
        let after_drop = audit_events_dropped();
        publish_audit_event(&ok_pub, &scope, event).await.expect("delivered publish");
        assert_eq!(
            audit_events_dropped(),
            after_drop,
            "a DELIVERED event incremented the drop counter"
        );
    }

    #[test]
    fn audit_topic_is_d22_canonical() {
        assert_eq!(AUDIT_TOPIC, "__lunaris_audit__");
    }
}
