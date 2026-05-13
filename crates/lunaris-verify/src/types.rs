//! Verifier DTOs — [`VerifyDecision`] (worker output) + [`VerifierBackend`]
//! (audit-event tag).
//!
//! Per D-11 the worker's `apply_supersede` consumes a [`VerifyDecision`] and
//! issues a single `StoragePort::atomic_write` with two `KvPut` ops (winner +
//! loser-with-new-sys-to). Per D-22 the same decision is serialized into an
//! `AuditEvent::VerifierArbitration { winner_id, loser_id, reason, backend,
//! decided_at_iso }` and published to `__lunaris_audit__` fire-and-forget.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// One verifier arbitration decision. `winner_id` / `loser_id` are `Option`
/// because a "deferred" decision (NoopVerifier short-circuit, or cloud-api
/// D-21 transient-after-retry) carries no arbitration pair — the worker
/// treats such decisions as "skip this item".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyDecision {
    pub winner_id: Option<Ulid>,
    pub loser_id: Option<Ulid>,
    pub reason: String,
    pub backend: VerifierBackend,
    pub decided_at_iso: String,
    /// Optional machine-readable cause for deferred decisions. Set by the
    /// cloud-api wrapper after D-21 retry exhaustion so the D-22 audit
    /// pipeline can distinguish "transient backend failure" from "NoopVerifier
    /// passthrough". Omitted from serialized audit records when `None`
    /// (additive, backwards-compatible — existing records round-trip fine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

impl VerifyDecision {
    /// Abstain — no arbitration produced. NoopVerifier returns this
    /// unconditionally; cloud-api returns this after D-21 retry exhaust.
    pub fn deferred() -> Self {
        Self {
            winner_id: None,
            loser_id: None,
            reason: "deferred (NoopVerifier or backend abstain)".to_string(),
            backend: VerifierBackend::Noop,
            decided_at_iso: chrono_iso_now(),
            cause: None,
        }
    }

    /// Abstain with a machine-readable cause and a specific backend tag. Used
    /// by the cloud-api wrapper (R3) to preserve the D-21 audit signal after
    /// retry exhaustion: `cause = "transient_after_retry: <err>"` and the
    /// correct provider `backend` tag flow through to the D-22 audit event.
    pub fn deferred_with_cause(cause: impl Into<String>, backend: VerifierBackend) -> Self {
        Self {
            winner_id: None,
            loser_id: None,
            reason: "deferred (transient backend failure after retry)".to_string(),
            backend,
            decided_at_iso: chrono_iso_now(),
            cause: Some(cause.into()),
        }
    }

    /// Real arbitration — winner + loser identified. The worker will issue
    /// ONE `atomic_write` superseding the loser and one audit publish.
    pub fn arbitrate(
        winner: Ulid,
        loser: Ulid,
        reason: impl Into<String>,
        backend: VerifierBackend,
    ) -> Self {
        Self {
            winner_id: Some(winner),
            loser_id: Some(loser),
            reason: reason.into(),
            backend,
            decided_at_iso: chrono_iso_now(),
            cause: None,
        }
    }

    /// `true` iff both `winner_id` and `loser_id` are set. The worker checks
    /// this before invoking `apply_supersede` so deferred decisions never
    /// trigger an MVCC write.
    pub fn applies(&self) -> bool {
        self.winner_id.is_some() && self.loser_id.is_some()
    }
}

/// Provider tag for audit records. Serialized with default enum tagging
/// (externally tagged → `"Candle"` / `"CloudAnthropic"` / etc.) so the audit
/// schema is grep-friendly in ops tooling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifierBackend {
    Candle,
    Ollama,
    CloudAnthropic,
    CloudOpenAI,
    CloudGemini,
    Noop,
}

/// W-4 fix: use chrono workspace dep instead of a hand-rolled date
/// decomposer. `chrono::Utc::now().to_rfc3339()` produces strings like
/// `"2026-04-20T15:00:00.123456789+00:00"` which deserialize losslessly
/// into any downstream consumer that expects RFC-3339.
fn chrono_iso_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_decision_round_trips_serde() {
        let d = VerifyDecision::arbitrate(
            Ulid::new(),
            Ulid::new(),
            "winner has higher confidence (0.9 > 0.6)",
            VerifierBackend::CloudAnthropic,
        );
        let bytes = serde_json::to_vec(&d).expect("serialize");
        let back: VerifyDecision = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(d, back);
        assert!(back.applies());
    }

    #[test]
    fn deferred_decision_does_not_apply() {
        let d = VerifyDecision::deferred();
        assert!(!d.applies());
        assert_eq!(d.backend, VerifierBackend::Noop);
        assert_eq!(d.winner_id, None);
        assert_eq!(d.loser_id, None);
    }

    #[test]
    fn verifier_backend_serializes_to_default_tags() {
        let json = serde_json::to_string(&VerifierBackend::Noop).unwrap();
        assert_eq!(json, "\"Noop\"");
        let json = serde_json::to_string(&VerifierBackend::CloudAnthropic).unwrap();
        assert_eq!(json, "\"CloudAnthropic\"");
    }

    #[test]
    fn iso_now_is_rfc3339() {
        let s = chrono_iso_now();
        // chrono::Utc::now().to_rfc3339() produces a string like
        // "2026-04-20T15:00:00.000+00:00" — verify the basic shape.
        assert!(s.len() >= 20, "rfc3339 len: {s}");
        assert!(s.contains('T'), "rfc3339 contains T separator: {s}");
    }
}
