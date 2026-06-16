//! RED-first (memory-update-intelligence) — the cross-episode arbitration arm.
//!
//! `cross_episode_decision` does not exist yet → compile-fail RED. Green lands
//! in build batch 5. The frozen contract routes cross-episode contradictions
//! THROUGH the verifier (NeedsReviewItem → verify() → arbitrate → supersede);
//! this pure helper is what each backend's CrossEpisodeContradiction arm calls,
//! so the convergence policy is deterministic + testable without model weights.

use lunaris_verify::{VerifierBackend, cross_episode_decision};
use ulid::Ulid;

#[test]
fn cross_episode_latest_assertion_supersedes_prior() {
    let existing = Ulid::new();
    let new = Ulid::new();

    let decision = cross_episode_decision(existing, new, VerifierBackend::Candle);

    assert!(
        decision.applies(),
        "a detected cross-episode contradiction must arbitrate, never defer"
    );
    assert_eq!(decision.winner_id, Some(new), "the newer assertion is the winner");
    assert_eq!(decision.loser_id, Some(existing), "the prior fact is the loser (superseded)");
    assert_eq!(decision.backend, VerifierBackend::Candle, "carries the deciding backend tag");
}
