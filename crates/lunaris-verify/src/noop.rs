//! [`NoopVerifier`] — passthrough that satisfies the trait without arbitration.
//!
//! Used as the default when:
//!
//! - The verify pipeline is OFF (Plan 04-04; default per blueprint §5.1 + D-02),
//!   in which case the worker is never spawned and `verify` is never called.
//! - No remote provider is configured (`LUNARIS_VERIFY_PROVIDER` unset —
//!   `default_verifier` substitutes
//!   `Arc::new(NoopVerifier)` with `tracing::warn!`). Mirrors the Plan 03-01
//!   `NoopExtractor` cache-miss fallback.
//!
//! `applies()` returns `false` so the worker loop (Plan 04-01 Task 3) can
//! short-circuit before calling `verify` entirely — saves one message
//! deserialize per passthrough tick.

use async_trait::async_trait;
use lunaris_core::LunarisError;

use crate::Verifier;
use crate::types::VerifyDecision;
use lunaris_extract::NeedsReviewItem;

/// Default verifier when the pipeline is OFF (Plan 04-04) or when no remote
/// backend cache is missing. Returns a [`VerifyDecision::deferred`] so the
/// worker treats the item as "abstain — leave the underlying primitive row
/// untouched".
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopVerifier;

#[async_trait]
impl Verifier for NoopVerifier {
    async fn verify(&self, _item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        Ok(VerifyDecision::deferred())
    }

    fn applies(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_extract::types::Entity;
    use lunaris_extract::{EntityId, NeedsReviewReason};

    #[tokio::test]
    async fn noop_returns_deferred_decision() {
        let v = NoopVerifier;
        let item = NeedsReviewItem::Entity {
            reason: NeedsReviewReason::InvalidBitemporal {
                valid_from: "2026-01-01T00:00:00Z".into(),
                valid_to: Some("2025-01-01T00:00:00Z".into()),
            },
            raw: Entity {
                id: EntityId::from_name_and_type("Alice", "Person"),
                name: "Alice".into(),
                aliases: Vec::new(),
                entity_type: "Person".into(),
                confidence: 0.5,
                valid_from_iso: "2026-01-01T00:00:00Z".into(),
                valid_to_iso: Some("2025-01-01T00:00:00Z".into()),
            },
        };
        let decision = v.verify(item).await.expect("noop never errs");
        assert!(!decision.applies());
        assert_eq!(decision.backend, crate::VerifierBackend::Noop);
    }

    #[test]
    fn noop_applies_false() {
        assert!(!NoopVerifier.applies(), "NoopVerifier::applies must be false");
    }
}
