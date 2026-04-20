//! Plan 04-05 D-22: unified audit topic `__lunaris_audit__`.
//!
//! Receives every `Lunaris::forget` call AND every verifier arbitration AND
//! every consolidator Episode→Fact promotion + archive. Fire-and-forget
//! publish — never blocks the caller (`tracing::warn!` on publish failure,
//! same pattern as `lunaris::ingest::publish_needs_review`).
//!
//! ## B-1 alignment (D-22 verbatim)
//!
//! Plan 04-01's verifier worker emits `kind="VerifierArbitration"` JSON to this
//! topic. Plan 04-02's consolidator worker emits per-promotion +
//! per-archive events with the kind tags `"ConsolidatorPromotion"` and
//! `"ConsolidatorArchive"`. This enum has matching variants — there is NO
//! rolled-up `ConsolidatorReport` variant (Plan 04-02 worker emits per-event
//! audits, NOT a rolled-up report envelope).
//!
//! ## Variant shapes (matched verbatim against worker JSON envelopes)
//!
//! | kind                        | Source                          | Variant                                                   |
//! |-----------------------------|---------------------------------|-----------------------------------------------------------|
//! | `Forget`                    | `Lunaris::forget` (this plan)   | `Forget(ForgetReceipt)`                                   |
//! | `VerifierArbitration`       | Plan 04-01 verifier worker      | `VerifierArbitration { winner_id, loser_id, reason, ... }` |
//! | `ConsolidatorPromotion`     | Plan 04-02 consolidator worker  | `ConsolidatorPromotion { episode_id, fact_id, ... }`      |
//! | `ConsolidatorArchive`       | Plan 04-02 consolidator worker  | `ConsolidatorArchive { fact_id, final_activation, ... }`  |

use std::sync::Arc;

use lunaris_core::StoragePort;
use lunaris_verify::VerifierBackend;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use lunaris_consolidate::FactId;

use crate::forget::ForgetReceipt;

/// Unified audit topic — every audit-emitting code path publishes here.
pub const AUDIT_TOPIC: &str = "__lunaris_audit__";

/// Canonical typed audit event. Externally-tagged so the JSON wire shape
/// carries `"kind": "<variant-name>"` for grep-friendly ops triage.
///
/// Per B-1 alignment with Plan 04-01 + Plan 04-02: 4 variants matching the
/// worker JSON envelopes verbatim. NO `ConsolidatorReport` rolled-up variant.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum AuditEvent {
    /// Emitted by `Lunaris::forget` after every successful call (D-22 + OPS-04).
    Forget(ForgetReceipt),

    /// Emitted by the lunaris-verify worker after every applied
    /// `VerifyDecision` (Plan 04-01 D-11 + D-22). The worker writes inline
    /// JSON with these field names; this typed variant captures the same
    /// shape so any consumer can deserialize either form.
    VerifierArbitration {
        /// Surviving primitive (`None` for deferred decisions; the worker
        /// short-circuits before emitting in that case but the option keeps
        /// the type total).
        winner_id: Option<String>,
        /// Superseded primitive.
        loser_id: Option<String>,
        /// Free-text reason from the model.
        reason: String,
        /// Backend tag for ops triage.
        backend: VerifierBackend,
        /// RFC-3339 wall-clock timestamp the decision was made at.
        decided_at_iso: String,
    },

    /// Per-promotion event (B-1 — Plan 04-02 worker emits ONE per
    /// Episode→Fact promotion). Replaces the rolled-up `ConsolidatorReport`
    /// variant that earlier drafts carried.
    ConsolidatorPromotion {
        episode_id: Ulid,
        fact_id: FactId,
        activation_score: f64,
    },

    /// Per-archive event (B-1 — Plan 04-02 worker emits ONE per archived Fact).
    ConsolidatorArchive {
        fact_id: FactId,
        final_activation: f64,
        moved_to: String,
    },
}

/// Fire-and-forget audit publish. Mirrors `lunaris::ingest::publish_needs_review`
/// (lines 397-415) verbatim:
///
/// 1. Try to serialize the event to JSON bytes.
/// 2. On serialize failure: `tracing::warn!`, return 0.
/// 3. On publish failure: `tracing::warn!`, return 0 — **never** propagate the
///    error. The caller's mutation already committed via `atomic_write`; an
///    audit-channel hiccup must not roll back the user's forget / verify /
///    consolidate write.
/// 4. On success: returns the broker-assigned offset (Lsn-equivalent).
///
/// Per blueprint §11 the audit channel is best-effort by design.
pub async fn publish_audit_event(storage: &Arc<dyn StoragePort>, event: AuditEvent) -> u64 {
    let payload = match serde_json::to_vec(&event) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(err = %e, "audit serialize failed; skipping audit publish");
            return 0;
        }
    };
    match storage.publish(AUDIT_TOPIC, 0, payload.into()).await {
        Ok(offset) => offset,
        Err(e) => {
            tracing::warn!(
                err = %e,
                "audit publish failed; caller mutation still succeeded"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forget::{ForgetReceipt, ForgetTarget, IndexKind, ScopeSpec};
    use lunaris_consolidate::FactId;
    use lunaris_core::storage::types::Lsn;
    use lunaris_extract::EntityId;

    #[test]
    fn audit_topic_constant_is_d22_canonical() {
        assert_eq!(AUDIT_TOPIC, "__lunaris_audit__");
    }

    #[test]
    fn forget_variant_serializes_with_kind_tag() {
        let receipt = ForgetReceipt {
            target: ForgetTarget::Scope(ScopeSpec::BySource("helios:fs/test/".to_string())),
            indices_affected: vec![IndexKind::Kv, IndexKind::Vector],
            rows_written: 5,
            rows_deleted: 0,
            audit_lsn: Lsn { wall_ms: 0, counter: 0 },
            preview: false,
        };
        let event = AuditEvent::Forget(receipt);
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json.get("kind").and_then(|v| v.as_str()), Some("Forget"));
    }

    #[test]
    fn verifier_arbitration_variant_matches_worker_envelope() {
        let event = AuditEvent::VerifierArbitration {
            winner_id: Some("01HKAPABCDEFGHIJKLMNOPQRST".to_string()),
            loser_id: Some("01HKAPABCDEFGHIJKLMNOPQRTV".to_string()),
            reason: "winner has higher confidence".to_string(),
            backend: VerifierBackend::CloudAnthropic,
            decided_at_iso: "2026-04-20T15:00:00.000Z".to_string(),
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json.get("kind").and_then(|v| v.as_str()),
            Some("VerifierArbitration")
        );
        assert_eq!(
            json.get("winner_id").and_then(|v| v.as_str()),
            Some("01HKAPABCDEFGHIJKLMNOPQRST")
        );
        assert_eq!(json.get("backend").and_then(|v| v.as_str()), Some("CloudAnthropic"));
    }

    /// B-1: ConsolidatorPromotion (per-promotion) replaces the dropped
    /// `ConsolidatorReport` rolled-up variant.
    #[test]
    fn consolidator_promotion_variant_matches_worker_envelope() {
        let event = AuditEvent::ConsolidatorPromotion {
            episode_id: Ulid::new(),
            fact_id: FactId::from_triple(EntityId([1; 16]), "knows", EntityId([2; 16])),
            activation_score: 1.5,
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json.get("kind").and_then(|v| v.as_str()),
            Some("ConsolidatorPromotion")
        );
        assert!(json.get("episode_id").is_some(), "episode_id field present");
        assert!(json.get("fact_id").is_some(), "fact_id field present");
        assert!(json.get("activation_score").is_some(), "activation_score field present");
    }

    #[test]
    fn consolidator_archive_variant_serializes() {
        let event = AuditEvent::ConsolidatorArchive {
            fact_id: FactId::from_triple(EntityId([1; 16]), "knows", EntityId([2; 16])),
            final_activation: -0.7,
            moved_to: "cold_storage".to_string(),
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json.get("kind").and_then(|v| v.as_str()),
            Some("ConsolidatorArchive")
        );
        assert!(json.get("moved_to").is_some());
    }

    /// B-1 regression guard: the rolled-up `ConsolidatorReport` variant must
    /// NOT exist. If a future refactor reintroduces it, this test fails.
    #[test]
    fn no_consolidator_report_variant_exists() {
        // Constructive proof: this test compiles iff the four variants above
        // are the only ones (any new variant would force an exhaustive match
        // pattern update across worker.rs + forget.rs + tests). Phase 4 contract
        // pin: 4 variants, no more.
        let receipt = ForgetReceipt {
            target: ForgetTarget::Id(Ulid::new()),
            indices_affected: vec![IndexKind::Kv],
            rows_written: 0,
            rows_deleted: 0,
            audit_lsn: Lsn { wall_ms: 0, counter: 0 },
            preview: true,
        };
        let event = AuditEvent::Forget(receipt);
        match event {
            AuditEvent::Forget(_) => {}
            AuditEvent::VerifierArbitration { .. } => {}
            AuditEvent::ConsolidatorPromotion { .. } => {}
            AuditEvent::ConsolidatorArchive { .. } => {}
        }
    }
}
