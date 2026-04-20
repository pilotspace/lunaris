//! Plan 04-02 Task 2 — ACT-R activation scorer per D-13.
//!
//! Anderson, J.R. (1996) "ACT: A simple theory of complex cognition."
//! _American Psychologist_ 51(4): 355–365 — base-level activation:
//!
//! ```text
//! A_i = ln( Σ_j t_j^(-d) ) + S_i + ε
//! ```
//!
//! where `t_j` are the elapsed times since each of the `j` references to
//! fact `i`, `d` is the decay parameter (default 0.5 per Anderson 1996),
//! `S_i` is a spreading-activation term from graph neighbors (D-14 default
//! 1-hop), and `ε` is activation noise (D-13 default 0.0 in v0; non-zero is
//! v2 CONSOL-V2-01).
//!
//! Petrov, A. (2006) "Computationally efficient approximation of the
//! base-level learning equation in ACT-R." gives the O(1) incremental update:
//! cache the running `Σ_j t_j^(-d)` per Fact and add the new `t_new^(-d)`
//! term on each new reference instead of recomputing the full history.
//! [`ActRScorer::update_with_new_reference`] implements this update.
//!
//! ## Defaults (D-13)
//!
//! [`ActRScorer::with_defaults`] uses [`ConsolidatorConfig::default`]:
//! `decay=0.5`, `archive_threshold=-0.5`, `promote_threshold=+1.0`,
//! `spreading_depth=1`, `noise=0.0`.

use crate::types::{ConsolidatorConfig, ReferenceTime};

/// Anderson 1996 base-level activation scorer with Petrov 2006 O(1)
/// incremental approximation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActRScorer {
    config: ConsolidatorConfig,
}

impl ActRScorer {
    /// New scorer with the given config.
    pub fn new(config: ConsolidatorConfig) -> Self {
        Self { config }
    }

    /// New scorer with [`ConsolidatorConfig::default`] — D-13 reference
    /// values (`decay=0.5`, `archive=-0.5`, `promote=+1.0`, 1-hop, no noise).
    pub fn with_defaults() -> Self {
        Self::new(ConsolidatorConfig::default())
    }

    /// Current config (for test assertions).
    pub fn config(&self) -> &ConsolidatorConfig {
        &self.config
    }

    /// Anderson 1996 base-level activation: `A_i = ln(Σ_j t_j^(-d)) + S_i + ε`.
    ///
    /// Returns `f64::NEG_INFINITY` when `refs` is empty (`ln(0) = -∞`) so a
    /// never-referenced Fact always scores below `archive_threshold`.
    ///
    /// `elapsed_seconds` is floored at 1 before the `t^(-d)` exponentiation
    /// to avoid `0^-0.5 = ∞` blowing up the running sum when an event arrives
    /// in the same second as its reference. Matches ACT-R convention that
    /// activation grows with time-since-reference, with a sensible 1-second
    /// floor at the boundary.
    pub fn activation(&self, refs: &[ReferenceTime], spreading: f64) -> f64 {
        if refs.is_empty() {
            return f64::NEG_INFINITY;
        }
        let sum: f64 = refs
            .iter()
            .map(|r| (r.elapsed_seconds.max(1) as f64).powf(-self.config.decay))
            .sum();
        sum.ln() + spreading + self.config.noise
    }

    /// Petrov 2006 O(1) incremental update. Given the previous
    /// `Σ_j t_j^(-d)` running sum (`prior_sum`) and a new reference arriving
    /// at `t_new` seconds ago, returns the updated sum. The caller then
    /// computes the new activation as `ln(updated_sum) + spreading + noise`.
    ///
    /// This is the production hot-path: the consolidator caches the running
    /// sum per Fact and avoids the O(refs.len()) full-history recompute on
    /// every new reference.
    pub fn update_with_new_reference(&self, prior_sum: f64, t_new: u64) -> f64 {
        prior_sum + (t_new.max(1) as f64).powf(-self.config.decay)
    }

    /// Returns `true` when `activation > promote_threshold` (D-13 default
    /// `+1.0`). The worker then issues an Episode→Fact promotion.
    pub fn should_promote(&self, activation: f64) -> bool {
        activation > self.config.promote_threshold
    }

    /// Returns `true` when `activation < archive_threshold` (D-13 default
    /// `-0.5`). The worker then archives the Fact out of the hot vector
    /// index into cold storage.
    pub fn should_archive(&self, activation: f64) -> bool {
        activation < self.config.archive_threshold
    }
}

impl Default for ActRScorer {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: Anderson 1996 formula for two references at t=60, t=120 with
    /// d=0.5 and no spreading / noise: `A = ln(60^-0.5 + 120^-0.5)`.
    #[test]
    fn activation_matches_anderson_1996_two_refs() {
        let s = ActRScorer::with_defaults();
        let refs = [ReferenceTime::new(60), ReferenceTime::new(120)];
        let a = s.activation(&refs, 0.0);
        // 60^-0.5 ≈ 0.12910, 120^-0.5 ≈ 0.09129, sum ≈ 0.22039, ln ≈ -1.5125
        let expected = (60_f64.powf(-0.5) + 120_f64.powf(-0.5)).ln();
        assert!(
            (a - expected).abs() < 1e-9,
            "activation = {a}, expected = {expected}"
        );
        // Cross-check the approximate literal from the plan:
        assert!(
            (a - (-1.514)).abs() < 1e-2,
            "activation ~= -1.514; got {a}"
        );
    }

    /// Test 2: Empty refs → `-∞`. The scorer's archive predicate will then
    /// trigger for any finite `archive_threshold`, which is every real config.
    #[test]
    fn activation_is_neg_infinity_on_empty_refs() {
        let s = ActRScorer::with_defaults();
        let a = s.activation(&[], 0.0);
        assert!(a.is_infinite() && a.is_sign_negative());
    }

    /// Test 3: Petrov 2006 incremental update equals the full-history
    /// recompute. Plan literal sanity check: `update(0.220, 30) ≈ 0.403`.
    #[test]
    fn petrov_2006_incremental_update_matches_forward_pass() {
        let s = ActRScorer::with_defaults();
        let prior = 60_f64.powf(-0.5); // ≈ 0.12910
        let updated = s.update_with_new_reference(prior, 30);
        let expected = prior + 30_f64.powf(-0.5);
        assert!((updated - expected).abs() < 1e-12);
        // The incremental path equals the full-history recompute:
        let full = s.activation(&[ReferenceTime::new(60), ReferenceTime::new(30)], 0.0);
        let incremental = updated.ln();
        assert!((full - incremental).abs() < 1e-12);

        // Plan literal: starting from a synthetic prior_sum=0.220 + new ref
        // t=30 (30^-0.5 ≈ 0.18257) → ≈ 0.40257.
        let updated_literal = s.update_with_new_reference(0.220, 30);
        assert!(
            (updated_literal - 0.40257).abs() < 1e-3,
            "plan literal expects ~0.403; got {updated_literal}"
        );
    }

    /// Test 4: `should_promote(1.5)==true` with default threshold 1.0.
    #[test]
    fn should_promote_fires_above_default_threshold() {
        let s = ActRScorer::with_defaults();
        assert!(s.should_promote(1.5));
        assert!(!s.should_promote(1.0), "strictly greater than, not >=");
        assert!(!s.should_promote(0.9));
    }

    /// Test 5: `should_archive(-0.7)==true` with default threshold -0.5.
    #[test]
    fn should_archive_fires_below_default_threshold() {
        let s = ActRScorer::with_defaults();
        assert!(s.should_archive(-0.7));
        assert!(!s.should_archive(-0.5), "strictly less than, not <=");
        assert!(!s.should_archive(-0.3));
    }

    /// Test 6: Spreading activation parameter is additive.
    #[test]
    fn spreading_activation_is_additive() {
        let s = ActRScorer::with_defaults();
        let refs = [ReferenceTime::new(60), ReferenceTime::new(120)];
        let base = s.activation(&refs, 0.0);
        let with_spread = s.activation(&refs, 1.25);
        assert!((with_spread - base - 1.25).abs() < 1e-12);
    }

    /// Test 7: Elapsed-seconds floor at 1 prevents `0^-0.5 = ∞` from
    /// blowing up the sum when an event arrives in the same second as its
    /// reference.
    #[test]
    fn zero_elapsed_seconds_floors_to_one() {
        let s = ActRScorer::with_defaults();
        let a = s.activation(&[ReferenceTime::new(0)], 0.0);
        // ln(1^-0.5) + 0 + 0 = ln(1) = 0
        assert!(a.abs() < 1e-12, "activation with floored t=1 = {a}, expected 0");
    }

    /// Test 8: Custom config — swap decay / thresholds — flows through every
    /// predicate.
    #[test]
    fn custom_config_overrides_flow_through_predicates() {
        let cfg = ConsolidatorConfig {
            decay: 0.3,
            archive_threshold: -1.0,
            promote_threshold: 2.5,
            spreading_depth: 2,
            noise: 0.1,
            community_rebuild_delta: 0.10,
        };
        let s = ActRScorer::new(cfg);
        assert_eq!(s.config().decay, 0.3);
        assert_eq!(s.config().promote_threshold, 2.5);
        assert!(!s.should_promote(2.0));
        assert!(s.should_promote(3.0));
        assert!(s.should_archive(-1.5));
        assert!(!s.should_archive(-0.75));
        // Activation respects the custom decay + noise terms:
        let a = s.activation(&[ReferenceTime::new(10)], 0.0);
        let expected = (10_f64.powf(-0.3)).ln() + 0.0 + 0.1;
        assert!((a - expected).abs() < 1e-12);
    }
}
