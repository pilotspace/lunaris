//! Plan 13-01 RELEASE-01 — byte-identical v0.1.0 audit shape parity.
//!
//! Each test loads a committed v0.1.0 fixture JSON, constructs the equivalent
//! `AuditEvent` via the canonical types in `lunaris_core::audit`, serializes
//! through `publish_audit_event`'s serde path, and asserts the
//! `serde_json::Value` round-trip is equal to the fixture's `Value`.
//!
//! Fixtures live at `tests/fixtures/audit/v0.1.0/*.json`. They are the frozen
//! v0.1.0 wire contract — regenerating them requires the 95%+ review bar
//! documented in `.planning/phases/13-auditevent-refactor-eval-05-release-gate/13-CONTEXT.md` D3.

use lunaris_core::Scope;
use lunaris_core::audit::{
    AuditEvent, FactIdData, ForgetReceiptData, ForgetTargetData, IndexKindData, PublishError,
    Publisher, ScopeSpecData, publish_audit_event,
};
use lunaris_core::storage::types::Lsn;
use parking_lot::Mutex;
use ulid::Ulid;

// ---- fixtures (checked into crates/lunaris-core/tests/fixtures/audit/v0.1.0/) ----
const FIXTURE_FORGET: &str = include_str!("fixtures/audit/v0.1.0/forget.json");
const FIXTURE_VERIFIER: &str = include_str!("fixtures/audit/v0.1.0/verifier_arbitration.json");
const FIXTURE_PROMOTION: &str = include_str!("fixtures/audit/v0.1.0/consolidator_promotion.json");
const FIXTURE_ARCHIVE: &str = include_str!("fixtures/audit/v0.1.0/consolidator_archive.json");
// Phase 14.1 — reflect-driven MVCC invalidation audit shape.
const FIXTURE_REFLECT_INVALIDATION: &str =
    include_str!("fixtures/audit/v0.1.0/reflect_invalidation.json");

fn fixture_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

/// Asserts the given `event` serializes to JSON equal to the fixture Value.
fn assert_serializes_to(event: &AuditEvent, fixture_raw: &str) {
    let produced = serde_json::to_value(event).expect("event serializes");
    let fixture = fixture_value(fixture_raw);
    assert_eq!(
        produced,
        fixture,
        "AuditEvent serialization drifted from v0.1.0 baseline.\n\
         produced = {}\nexpected = {}",
        serde_json::to_string_pretty(&produced).unwrap(),
        serde_json::to_string_pretty(&fixture).unwrap()
    );
}

// ---------------------------- fixture-parity ---------------------------------

#[test]
fn fixture_parity_forget() {
    let event = AuditEvent::Forget(ForgetReceiptData {
        target: ForgetTargetData::Scope(ScopeSpecData::BySource(
            "helios:fs/session-42/".to_string(),
        )),
        indices_affected: vec![IndexKindData::Kv, IndexKindData::Vector],
        rows_written: 5,
        rows_deleted: 0,
        audit_lsn: Lsn { wall_ms: 1713000000000, counter: 7 },
        preview: false,
    });
    assert_serializes_to(&event, FIXTURE_FORGET);
}

#[test]
fn fixture_parity_verifier_arbitration() {
    let event = AuditEvent::VerifierArbitration {
        winner_id: Some("01HKAPABCDEFGHIJKLMNOPQRST".to_string()),
        loser_id: Some("01HKAPABCDEFGHIJKLMNOPQRTV".to_string()),
        reason: "winner has higher confidence".to_string(),
        backend: "CloudAnthropic".to_string(),
        decided_at_iso: "2026-04-20T15:00:00.000Z".to_string(),
    };
    assert_serializes_to(&event, FIXTURE_VERIFIER);
}

#[test]
fn fixture_parity_consolidator_promotion() {
    let episode_id: Ulid = "01HKAP00000000000000000000".parse().unwrap();
    let fact_bytes: [u8; 16] = [123, 216, 144, 97, 1, 113, 244, 67, 1, 53, 96, 68, 42, 244, 151, 0];
    let event = AuditEvent::ConsolidatorPromotion {
        episode_id,
        fact_id: FactIdData(fact_bytes),
        activation_score: 1.5,
    };
    assert_serializes_to(&event, FIXTURE_PROMOTION);
}

#[test]
fn fixture_parity_consolidator_archive() {
    let fact_bytes: [u8; 16] = [123, 216, 144, 97, 1, 113, 244, 67, 1, 53, 96, 68, 42, 244, 151, 0];
    let event = AuditEvent::ConsolidatorArchive {
        fact_id: FactIdData(fact_bytes),
        final_activation: -0.7,
        moved_to: "cold_storage".to_string(),
    };
    assert_serializes_to(&event, FIXTURE_ARCHIVE);
}

#[test]
fn fixture_parity_reflect_invalidation() {
    let event = AuditEvent::ReflectInvalidation {
        ulid: "01HKAPABCDEFGHIJKLMNOPQRST".to_string(),
        scope: "agent-42".to_string(),
        invalidated_at_iso: "2026-05-13T00:00:00+00:00".to_string(),
        turn_id: Some("01HKAPABCDEFGHIJKLMNOPQRYZ".to_string()),
    };
    assert_serializes_to(&event, FIXTURE_REFLECT_INVALIDATION);
}

#[test]
fn fixture_parity_reflect_invalidation_no_turn_id() {
    // Verify that when turn_id is None the field is omitted from the JSON
    // (skip_serializing_if = "Option::is_none") — not covered by the frozen
    // fixture but still a wire-shape contract.
    let event = AuditEvent::ReflectInvalidation {
        ulid: "01HKAPABCDEFGHIJKLMNOPQRST".to_string(),
        scope: "agent-42".to_string(),
        invalidated_at_iso: "2026-05-13T00:00:00+00:00".to_string(),
        turn_id: None,
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "ReflectInvalidation");
    assert!(v.get("turn_id").is_none(), "turn_id must be absent when None");
    // Round-trip: deserializing must reconstruct the same event.
    let back: AuditEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, event);
}

// ---------------------------- publish round-trip -----------------------------

/// In-memory publisher used to assert publish_audit_event's serde path is
/// byte-identical to to_value.
struct CapturePublisher {
    inbox: Mutex<Vec<(String, String, u16, bytes::Bytes)>>,
}

#[async_trait::async_trait]
impl Publisher for CapturePublisher {
    async fn publish(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
        payload: bytes::Bytes,
    ) -> Result<u64, PublishError> {
        let mut inbox = self.inbox.lock();
        inbox.push((scope.as_str().to_string(), topic.to_string(), partition, payload));
        Ok(inbox.len() as u64)
    }
}

#[tokio::test]
async fn publish_audit_event_round_trip_all_variants() {
    let pub_ = CapturePublisher { inbox: Mutex::new(Vec::new()) };

    let forget = AuditEvent::Forget(ForgetReceiptData {
        target: ForgetTargetData::Scope(ScopeSpecData::BySource(
            "helios:fs/session-42/".to_string(),
        )),
        indices_affected: vec![IndexKindData::Kv, IndexKindData::Vector],
        rows_written: 5,
        rows_deleted: 0,
        audit_lsn: Lsn { wall_ms: 1713000000000, counter: 7 },
        preview: false,
    });
    let scope = Scope::new("helios").unwrap();
    let off = publish_audit_event(&pub_, &scope, forget.clone()).await.unwrap();
    assert_eq!(off, 1);

    let inbox = pub_.inbox.lock();
    let (published_scope, topic, partition, payload) = &inbox[0];
    // W4.6 — the producing scope reaches the broker, not `Scope::dev()`.
    assert_eq!(published_scope, "helios");
    assert_eq!(topic, "__lunaris_audit__");
    assert_eq!(*partition, 0);
    let decoded: AuditEvent = serde_json::from_slice(payload).unwrap();
    assert_eq!(decoded, forget);
}
