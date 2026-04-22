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
pub enum IndexKindData {
    Kv,
    Vector,
    Graph,
}

/// Mirror of `lunaris::forget::ScopeSpec`. External-tag serialization ⇒
/// `{"BySource":"..."}` / `{"ByMetadata":["k","v"]}` / `{"ByEpisode":"<ulid>"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeSpecData {
    BySource(String),
    ByMetadata(String, String),
    ByEpisode(Ulid),
}

/// Mirror of `lunaris::forget::ForgetTarget`. External-tag serialization ⇒
/// `{"Id":"<ulid>"}` / `{"Scope":{...}}` / `{"Before":{<hlc>}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    ConsolidatorPromotion {
        episode_id: Ulid,
        fact_id: FactIdData,
        activation_score: f64,
    },

    /// Per-archive event (one per archived Fact).
    ConsolidatorArchive {
        fact_id: FactIdData,
        final_activation: f64,
        moved_to: String,
    },
}

// ---------------------------------------------------------------------------
// Publisher trait + blanket impl for Arc<dyn StoragePort>
// ---------------------------------------------------------------------------

/// Publish error returned by [`publish_audit_event`]. Always logged via
/// `tracing::warn!` — callers may ignore the error per the fire-and-forget
/// contract (blueprint §11).
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("audit serialize failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("audit publish backend failed: {0}")]
    Backend(String),
}

/// Narrow publish surface. Decouples [`publish_audit_event`] from
/// [`StoragePort`] so tests can substitute an in-memory capture without
/// implementing the full `StoragePort` trait.
#[async_trait]
pub trait Publisher: Send + Sync {
    async fn publish(
        &self,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, PublishError>;
}

#[async_trait]
impl Publisher for Arc<dyn StoragePort> {
    async fn publish(
        &self,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, PublishError> {
        StoragePort::publish(self.as_ref(), topic, partition, payload)
            .await
            .map_err(|e: StorageError| PublishError::Backend(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// publish_audit_event helper (fire-and-forget; tracing::warn! on failure)
// ---------------------------------------------------------------------------

/// Fire-and-forget audit publish. Mirrors the pre-refactor helper at
/// `crates/lunaris/src/audit.rs:99-117` verbatim:
///
/// 1. Try to serialize the event to JSON bytes.
/// 2. On serialize failure: `tracing::warn!`, return `Ok(0)`.
/// 3. On publish failure: `tracing::warn!`, return `Ok(0)` — **never**
///    propagate. The caller's mutation already committed via `atomic_write`;
///    an audit-channel hiccup must not roll back the user's write.
/// 4. On success: returns the broker-assigned offset.
///
/// Returns `Ok(u64)` on both success AND soft-failure so existing callers
/// that bind the offset (e.g. `ForgetReceipt::audit_lsn`) keep working.
/// The `PublishError` variant is reserved for future strict-mode callers.
pub async fn publish_audit_event<P: Publisher + ?Sized>(
    publisher: &P,
    event: AuditEvent,
) -> Result<u64, PublishError> {
    let payload = match serde_json::to_vec(&event) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(err = %e, "audit serialize failed; skipping audit publish");
            return Ok(0);
        }
    };
    match publisher.publish(AUDIT_TOPIC, 0, payload.into()).await {
        Ok(offset) => Ok(offset),
        Err(e) => {
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
        pub inbox: Mutex<Vec<(String, u16, Bytes)>>,
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
            topic: &str,
            partition: u16,
            payload: Bytes,
        ) -> Result<u64, PublishError> {
            let mut box_ = self.inbox.lock();
            box_.push((topic.to_string(), partition, payload));
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
        let off = publish_audit_event(&pub_, event.clone()).await.unwrap();
        assert_eq!(off, 1);
        let inbox = pub_.inbox.lock();
        assert_eq!(inbox[0].0, AUDIT_TOPIC);
        let decoded: AuditEvent = serde_json::from_slice(&inbox[0].2).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn audit_topic_is_d22_canonical() {
        assert_eq!(AUDIT_TOPIC, "__lunaris_audit__");
    }
}
