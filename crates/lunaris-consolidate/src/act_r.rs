//! Plan 04-02 Task 2 — ACT-R activation scorer (Anderson 1996 + Petrov 2006).
//!
//! B-7 forward-compat stub: Task 1 commits this file as a minimal stub with
//! the public [`ActRScorer`] type so `cargo check -p lunaris-consolidate`
//! passes after Task 1 alone. Task 2 replaces the body with the full
//! Anderson 1996 + Petrov 2006 implementation + unit tests while preserving
//! the public signature.

use crate::types::{ConsolidatorConfig, ReferenceTime};

/// Anderson 1996 base-level activation scorer with Petrov 2006 O(1)
/// incremental approximation.
///
/// ```text
/// A_i = ln( Σ_j t_j^(-d) ) + S_i + ε
/// ```
///
/// **Task 1 stub:** constructors + config accessor + predicate signatures
/// present; Task 2 replaces the `activation` / `update_with_new_reference`
/// bodies with the real math and adds the 8 unit tests.
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

    /// Anderson 1996 base-level activation (Task 2 body).
    pub fn activation(&self, _refs: &[ReferenceTime], _spreading: f64) -> f64 {
        unimplemented!("Task 2 replaces this stub with Anderson 1996 formula")
    }

    /// Petrov 2006 O(1) incremental update (Task 2 body).
    pub fn update_with_new_reference(&self, _prior_sum: f64, _t_new: u64) -> f64 {
        unimplemented!("Task 2 replaces this stub with Petrov 2006 incremental update")
    }

    /// Returns `true` when `activation > promote_threshold` (D-13 default `+1.0`).
    pub fn should_promote(&self, activation: f64) -> bool {
        activation > self.config.promote_threshold
    }

    /// Returns `true` when `activation < archive_threshold` (D-13 default `-0.5`).
    pub fn should_archive(&self, activation: f64) -> bool {
        activation < self.config.archive_threshold
    }
}

impl Default for ActRScorer {
    fn default() -> Self {
        Self::with_defaults()
    }
}
