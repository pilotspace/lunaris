//! RFC 0006 §4 — verifier divergence comparison harness.
//!
//! Compares two `Verifier` implementations over a fixture set of
//! [`NeedsReviewItem`]s and reports the divergence rate. The RFC 0006
//! default-flip gate (27B → 270M) requires divergence ≤ 5% on a 100-item
//! corpus before the laptop-floor verifier becomes the umbrella default.
//!
//! ## Divergence definition (RFC 0006 §4.2)
//!
//! Two decisions are **equivalent** iff:
//! 1. Both `applies()` returns the same boolean (no false-arbitrate vs
//!    abstain mismatch), AND
//! 2. When both apply, the `winner_id` matches AND the `loser_id` matches.
//!    The `reason` string is intentionally NOT compared (free-form,
//!    LLM-dependent).
//! 3. When both abstain (`!applies()`), they are equivalent regardless of
//!    `reason`. A deferred-vs-deferred case is a tie, not a divergence.
//!
//! Divergence = (1 - matches / total). Range: `[0.0, 1.0]`.
//!
//! ## Why this is in `lunaris-verify` and not `lunaris-bench`
//!
//! The harness is **pure** — it consumes the `Verifier` trait, makes no
//! filesystem assumptions, runs in any tokio runtime. Putting it here
//! lets downstream callers (including `lunaris-bench`'s comparison
//! binary) reuse it without circular deps. The `lunaris-bench` crate
//! adds the fixture corpus and the binary that actually loads Candle
//! weights.

use std::sync::Arc;

use lunaris_core::LunarisError;

use crate::{NeedsReviewItem, VerifyDecision, Verifier};

/// One row in the divergence-comparison output. Carries the underlying
/// item + both verifier decisions so a UI / debug print can show "here's
/// exactly what disagreed."
#[derive(Clone, Debug)]
pub struct DivergenceCase {
    pub item: NeedsReviewItem,
    pub decision_a: VerifyDecision,
    pub decision_b: VerifyDecision,
    pub diverged: bool,
}

/// Aggregate report. RFC 0006 §4 fail condition: `divergence_rate > 0.05`
/// on a 100-item corpus.
#[derive(Clone, Debug)]
pub struct DivergenceReport {
    pub total: usize,
    pub matches: usize,
    pub diverged: usize,
    pub divergence_rate: f64,
    pub cases: Vec<DivergenceCase>,
}

impl DivergenceReport {
    /// `true` iff `divergence_rate <= threshold`. The RFC 0006 §4 default
    /// flip threshold is 0.05 (5%).
    #[must_use]
    pub fn passes(&self, threshold: f64) -> bool {
        self.divergence_rate <= threshold
    }
}

/// Decide whether two [`VerifyDecision`]s are equivalent under the RFC 0006
/// §4.2 rule. `reason` strings are intentionally ignored.
#[must_use]
pub fn decisions_equivalent(a: &VerifyDecision, b: &VerifyDecision) -> bool {
    match (a.applies(), b.applies()) {
        (false, false) => true,
        (true, true) => a.winner_id == b.winner_id && a.loser_id == b.loser_id,
        _ => false,
    }
}

/// Run both verifiers over `items` and produce a [`DivergenceReport`].
///
/// Sequential execution (one item at a time, A then B) — divergence is
/// not a latency-bound metric, and sequential keeps the verifier worker's
/// in-flight state predictable. If a future comparison needs parallelism
/// it can `futures::stream::iter().for_each_concurrent` over `verify`.
///
/// Any error from either verifier is **surfaced** rather than swallowed —
/// the RFC 0006 §4 gate must run on a corpus where both verifiers respond
/// successfully. If you need partial-failure tolerance, pre-filter the
/// corpus.
pub async fn compare_verifiers(
    verifier_a: Arc<dyn Verifier>,
    verifier_b: Arc<dyn Verifier>,
    items: Vec<NeedsReviewItem>,
) -> Result<DivergenceReport, LunarisError> {
    let total = items.len();
    let mut cases = Vec::with_capacity(total);
    let mut diverged = 0usize;

    for item in items {
        let decision_a = verifier_a.verify(item.clone()).await?;
        let decision_b = verifier_b.verify(item.clone()).await?;
        let case_diverged = !decisions_equivalent(&decision_a, &decision_b);
        if case_diverged {
            diverged += 1;
        }
        cases.push(DivergenceCase { item, decision_a, decision_b, diverged: case_diverged });
    }

    let matches = total - diverged;
    let divergence_rate = if total == 0 { 0.0 } else { diverged as f64 / total as f64 };

    Ok(DivergenceReport { total, matches, diverged, divergence_rate, cases })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VerifierBackend;
    use async_trait::async_trait;
    use ulid::Ulid;

    /// A scripted verifier — pop decisions off the front of a queue. Lets
    /// the divergence math be exercised without loading a real model.
    struct ScriptedVerifier {
        decisions: parking_lot::Mutex<std::collections::VecDeque<VerifyDecision>>,
    }
    impl ScriptedVerifier {
        fn new(decisions: Vec<VerifyDecision>) -> Arc<Self> {
            Arc::new(Self { decisions: parking_lot::Mutex::new(decisions.into()) })
        }
    }
    #[async_trait]
    impl Verifier for ScriptedVerifier {
        async fn verify(&self, _item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
            Ok(self.decisions.lock().pop_front().expect("script underflow"))
        }
    }

    fn arbitrate(winner: Ulid, loser: Ulid) -> VerifyDecision {
        VerifyDecision::arbitrate(winner, loser, "test", VerifierBackend::Candle)
    }

    fn nri() -> NeedsReviewItem {
        // The harness ignores the item body — build a minimal Entity-shaped
        // NeedsReviewItem to satisfy the type. If lunaris-extract's Entity
        // shape changes upstream this fixture gets a single-line update.
        use lunaris_extract::types::{Entity, EntityId};
        use lunaris_extract::NeedsReviewReason;
        NeedsReviewItem::Entity {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "test".to_string(),
                error: "stub".to_string(),
            },
            raw: Entity {
                id: EntityId::from_name_and_type("Test Entity", "Person"),
                name: "Test Entity".to_string(),
                aliases: vec![],
                entity_type: "Person".to_string(),
                confidence: 0.99,
                valid_from_iso: "2026-01-01T00:00:00Z".to_string(),
                valid_to_iso: None,
            },
        }
    }

    #[tokio::test]
    async fn empty_corpus_has_zero_divergence() {
        let a = ScriptedVerifier::new(vec![]);
        let b = ScriptedVerifier::new(vec![]);
        let report = compare_verifiers(a, b, vec![]).await.expect("ok");
        assert_eq!(report.total, 0);
        assert_eq!(report.divergence_rate, 0.0);
        assert!(report.passes(0.05));
    }

    #[tokio::test]
    async fn identical_decisions_yield_zero_divergence() {
        let w = Ulid::new();
        let l = Ulid::new();
        let a = ScriptedVerifier::new(vec![arbitrate(w, l), arbitrate(w, l)]);
        let b = ScriptedVerifier::new(vec![arbitrate(w, l), arbitrate(w, l)]);
        let report = compare_verifiers(a, b, vec![nri(), nri()]).await.expect("ok");
        assert_eq!(report.matches, 2);
        assert_eq!(report.diverged, 0);
        assert_eq!(report.divergence_rate, 0.0);
        assert!(report.passes(0.05));
    }

    #[tokio::test]
    async fn winner_swap_counts_as_divergence() {
        let w1 = Ulid::new();
        let l1 = Ulid::new();
        let a = ScriptedVerifier::new(vec![arbitrate(w1, l1)]);
        let b = ScriptedVerifier::new(vec![arbitrate(l1, w1)]); // winner+loser swapped
        let report = compare_verifiers(a, b, vec![nri()]).await.expect("ok");
        assert_eq!(report.diverged, 1);
        assert_eq!(report.divergence_rate, 1.0);
        assert!(!report.passes(0.05));
    }

    #[tokio::test]
    async fn deferred_vs_deferred_is_tie() {
        let a = ScriptedVerifier::new(vec![VerifyDecision::deferred()]);
        let b = ScriptedVerifier::new(vec![VerifyDecision::deferred()]);
        let report = compare_verifiers(a, b, vec![nri()]).await.expect("ok");
        assert_eq!(report.diverged, 0, "deferred-vs-deferred is a tie, not a divergence");
    }

    #[tokio::test]
    async fn arbitrate_vs_defer_diverges() {
        let w = Ulid::new();
        let l = Ulid::new();
        let a = ScriptedVerifier::new(vec![arbitrate(w, l)]);
        let b = ScriptedVerifier::new(vec![VerifyDecision::deferred()]);
        let report = compare_verifiers(a, b, vec![nri()]).await.expect("ok");
        assert_eq!(report.diverged, 1);
    }

    #[test]
    fn reason_string_difference_is_ignored() {
        let w = Ulid::new();
        let l = Ulid::new();
        let a = VerifyDecision::arbitrate(w, l, "reason A", VerifierBackend::Candle);
        let b = VerifyDecision::arbitrate(w, l, "totally different prose", VerifierBackend::Candle);
        assert!(decisions_equivalent(&a, &b), "reason strings must not affect equivalence");
    }
}
