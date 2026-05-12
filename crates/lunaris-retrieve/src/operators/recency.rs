//! Recency rescorer — P0 #3 (Wave 1) — RED skeleton.
//!
//! Failing tests for the pluggable recency rescorer. The implementation
//! lands in the next commit; this file establishes the public surface
//! (config, scorer trait, `rescore_recency` entrypoint) and the
//! red/green TDD contract.

use std::time::Duration;

use lunaris_core::Hlc;

use crate::types::Hit;

/// Which Hit timestamp the rescorer reads. See module doc on the GREEN
/// commit for the full rationale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TimeSource {
    #[default]
    ValidFrom,
    SysFrom,
}

/// Pluggable decay curve. Implementations receive the hit age (seconds,
/// clamped non-negative) and the prior score, and return the new score.
pub trait RecencyScorer: Send + Sync {
    fn decay(&self, age_seconds: f32, prior_score: f32) -> f32;
}

/// Exponential half-life decay — stub for RED commit.
#[derive(Clone, Copy, Debug)]
pub struct Exp {
    pub half_life: Duration,
}

impl Exp {
    pub fn new(half_life: Duration) -> Self {
        Self { half_life }
    }
}

impl RecencyScorer for Exp {
    fn decay(&self, _age_seconds: f32, prior_score: f32) -> f32 {
        // RED: returns the prior score unchanged so the "boosts newer"
        // and "half-life" tests fail.
        prior_score
    }
}

/// ACT-R scorer — stub for RED commit.
#[derive(Clone, Copy, Debug)]
pub struct ActR {
    pub decay: f32,
}

pub const ACT_R_MIN_AGE_SECONDS: f32 = 1.0;

impl Default for ActR {
    fn default() -> Self {
        Self { decay: 0.5 }
    }
}

impl ActR {
    pub fn new(decay: f32) -> Self {
        Self { decay }
    }
}

impl RecencyScorer for ActR {
    fn decay(&self, _age_seconds: f32, prior_score: f32) -> f32 {
        // RED: parity test fails until GREEN commit implements the
        // Anderson (1996) base-level formula.
        prior_score
    }
}

pub struct RecencyConfig {
    pub source: TimeSource,
    pub scorer: Box<dyn RecencyScorer>,
}

impl Default for RecencyConfig {
    fn default() -> Self {
        Self {
            source: TimeSource::ValidFrom,
            scorer: Box::new(Exp::new(Duration::from_secs(60 * 60 * 24 * 7))),
        }
    }
}

impl RecencyConfig {
    pub fn new(source: TimeSource, scorer: Box<dyn RecencyScorer>) -> Self {
        Self { source, scorer }
    }
}

/// Apply the recency rescorer in-place. RED stub — does nothing.
pub fn rescore_recency(_hits: &mut Vec<Hit>, _now: Hlc, _config: &RecencyConfig) {
    // GREEN commit replaces this body with the real rescore + sort.
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{Hit, SourceOp};

    fn mk_hit(id: u8, score: f32, valid_from_ms: u64) -> Hit {
        Hit {
            id: vec![id],
            score,
            text: String::new(),
            source: String::new(),
            heading_path: Vec::new(),
            valid_from: Hlc { wall_ms: valid_from_ms, counter: 0, node_id: 0 },
            valid_to: None,
            degraded: false,
            rerank_applied: false,
            source_op: SourceOp::Fused,
        }
    }

    fn now(ms: u64) -> Hlc {
        Hlc { wall_ms: ms, counter: 0, node_id: 0 }
    }

    #[test]
    fn recency_preserves_order_for_simultaneous_hits() {
        let mut hits =
            vec![mk_hit(1, 0.7, 1_000_000), mk_hit(2, 0.7, 1_000_000), mk_hit(3, 0.7, 1_000_000)];
        let cfg = RecencyConfig::default();
        rescore_recency(&mut hits, now(2_000_000), &cfg);
        let ids: Vec<u8> = hits.iter().map(|h| h.id[0]).collect();
        assert_eq!(ids, vec![1, 2, 3], "stable sort under tied scores");
    }

    #[test]
    fn recency_boosts_newer_hits() {
        let now_ms: u64 = 10_000_000_000;
        let thirty_days_ms: u64 = 30 * 24 * 3600 * 1000;
        let mut hits = vec![
            mk_hit(99, 0.5, now_ms - thirty_days_ms),
            mk_hit(7, 0.5, now_ms),
        ];
        let cfg = RecencyConfig::default();
        rescore_recency(&mut hits, now(now_ms), &cfg);
        assert_eq!(hits[0].id, vec![7], "newer hit should rank first");
        assert_eq!(hits[1].id, vec![99]);
        assert!(
            hits[0].score > hits[1].score,
            "newer={:?} should exceed older={:?}",
            hits[0].score,
            hits[1].score
        );
    }

    #[test]
    fn recency_exp_half_life_matches_formula() {
        let half_life = Duration::from_secs(3600);
        let now_ms = 1_000_000_000;
        let one_hl_ago = now_ms - half_life.as_millis() as u64;
        let mut hits = vec![mk_hit(1, 1.0, one_hl_ago)];
        let cfg = RecencyConfig::new(TimeSource::ValidFrom, Box::new(Exp::new(half_life)));
        rescore_recency(&mut hits, now(now_ms), &cfg);
        assert!(
            (hits[0].score - 0.5).abs() < 1e-5,
            "at one half-life expected 0.5, got {}",
            hits[0].score
        );
    }

    #[test]
    fn recency_zero_age_is_identity_for_exp() {
        let mut hits = vec![mk_hit(1, 0.42, 5_000_000)];
        let cfg = RecencyConfig::new(
            TimeSource::ValidFrom,
            Box::new(Exp::new(Duration::from_secs(3600))),
        );
        rescore_recency(&mut hits, now(5_000_000), &cfg);
        assert!(
            (hits[0].score - 0.42).abs() < 1e-6,
            "zero-age Exp should be identity, got {}",
            hits[0].score
        );
    }

    #[test]
    fn recency_act_r_matches_message_stream_formula() {
        let age_s = 60.0_f32;
        let prior = 0.3_f32;
        let scorer = ActR::new(0.5);
        let got = scorer.decay(age_s, prior);
        let expected = prior + age_s.powf(-0.5).ln();
        assert!((got - expected).abs() < 1e-5, "got {got}, expected {expected}");
    }

    #[test]
    fn recency_clock_skew_clamps_age_to_zero() {
        let mut hits = vec![mk_hit(1, 0.9, 2_000_000)];
        let cfg = RecencyConfig::new(
            TimeSource::ValidFrom,
            Box::new(Exp::new(Duration::from_secs(3600))),
        );
        rescore_recency(&mut hits, now(1_000_000), &cfg);
        assert!(
            (hits[0].score - 0.9).abs() < 1e-6,
            "clock-skew age clamp expected identity, got {}",
            hits[0].score
        );
    }

    #[test]
    fn recency_zero_valid_from_is_passthrough() {
        let mut hits = vec![mk_hit(1, 0.77, 0)];
        let cfg = RecencyConfig::default();
        rescore_recency(&mut hits, now(1_000_000_000), &cfg);
        assert!(
            (hits[0].score - 0.77).abs() < 1e-6,
            "Hlc::ZERO should pass through, got {}",
            hits[0].score
        );
    }

    #[test]
    fn recency_empty_hits_is_noop() {
        let mut hits: Vec<Hit> = Vec::new();
        let cfg = RecencyConfig::default();
        rescore_recency(&mut hits, now(123), &cfg);
        assert!(hits.is_empty());
    }
}
