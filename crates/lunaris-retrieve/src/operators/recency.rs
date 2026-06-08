//! Recency rescorer — P0 #3 (Wave 1).
//!
//! Promotes the recency-weighting pattern from `lunaris-recipes::MessageStream`
//! into a first-class, reusable operator-layer rescorer. The recipe blends
//! ACT-R base-level activation with a fused RRF score; this module generalises
//! that to a pluggable [`RecencyScorer`] trait so callers can pick exponential
//! half-life decay (default), ACT-R, or a custom curve.
//!
//! ## Design — three alternatives, recommended path
//!
//! 1. Hard-coded ACT-R blend (`.recency_act_r()`). Simple but inflexible —
//!    every new decay shape forces a new method.
//! 2. Single configurable knob (`.recency(half_life)`). Better, but bakes in
//!    one curve.
//! 3. Pluggable scorer trait (this module): `RecencyConfig { source, scorer }`
//!    with [`Exp`] as the default and [`ActR`] as the parity scorer for the
//!    recipe layer. Default config: `TimeSource::ValidFrom` +
//!    `Exp { half_life = 7 days }`.
//!
//! ## Why post-hydrate?
//!
//! `RawHit` does not carry `valid_from`; the bi-temporal stamp only lands on
//! the `Hit` after [`crate::hydrate::hydrate`] looks the chunk up. Two
//! candidate placements:
//!
//! - (a) Add `valid_from: Option<Hlc>` to `RawHit`. Requires every backend's
//!   `vector_search` / `keyword_search` to populate the field — a wide,
//!   cross-crate change.
//! - (b) Run the rescorer AFTER hydrate, on `Vec<Hit>` (this module).
//!
//! We pick (b). It is additive — no `RawHit` field, no `Retriever` trait
//! change, no `SourceOp` variant. The public entrypoint is the free function
//! [`rescore_recency`]; wiring it into [`crate::builder::RetrievalBuilder`]
//! via `.recency(config)` is intentionally deferred to a follow-up commit on
//! main (P0 #2 — parallel agent — owns `builder.rs` this round).
//!
//! ## Edge cases
//!
//! - `valid_from == Hlc::ZERO` — treat as "no recency signal"; pass-through
//!   the prior score unchanged so a deleted/unstamped fact does not get a
//!   spurious decay penalty.
//! - `now.wall_ms < valid_from.wall_ms` (clock skew) — clamp age to zero
//!   via `saturating_sub`.
//! - NaN scorer output — guarded by [`f32::is_nan`]; falls back to the prior
//!   score so the sort never destabilises.
//! - Empty hits — no-op.

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

/// Exponential half-life decay: `score' = score * 2^(-age / half_life)`.
///
/// Equivalent to `score * exp(-ln(2) * age / half_life)`. At `age = 0` the
/// score is unchanged; at `age = half_life` the score is exactly halved;
/// at `age = 2 * half_life` the score is quartered.
///
/// Default in [`RecencyConfig::default`]: `half_life = 7 days`. Wide enough
/// not to upend the relative ordering of week-old hits, narrow enough to
/// noticeably penalise month-old ones.
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
    fn decay(&self, age_seconds: f32, prior_score: f32) -> f32 {
        let hl = self.half_life.as_secs_f32();
        if hl <= 0.0 {
            // Degenerate config — refuse to penalise.
            return prior_score;
        }
        let factor = (-age_seconds / hl).exp2();
        prior_score * factor
    }
}

/// ACT-R base-level activation per Anderson (1996):
/// `B(i) = ln(age^-decay)` for a single access; blended additively with
/// the prior score to mirror `lunaris_recipes::MessageStream::recall`.
///
/// `decay` defaults to `0.5` per Anderson (1996). Ages below
/// [`ACT_R_MIN_AGE_SECONDS`] are clamped so `age.powf(-decay)` never
/// explodes when a fact is recalled within the same wall-millisecond it
/// was ingested (this is the same clamp `MessageStream` applies via its
/// `MIN_AGE_SECONDS` constant).
#[derive(Clone, Copy, Debug)]
pub struct ActR {
    pub decay: f32,
}

/// Lower bound on age (seconds) for the [`ActR`] scorer — see [`ActR`].
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
    fn decay(&self, age_seconds: f32, prior_score: f32) -> f32 {
        let clamped = age_seconds.max(ACT_R_MIN_AGE_SECONDS);
        let activation = clamped.powf(-self.decay).ln();
        prior_score + activation
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

    pub fn with_source(mut self, source: TimeSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_scorer(mut self, scorer: Box<dyn RecencyScorer>) -> Self {
        self.scorer = scorer;
        self
    }
}

/// Read the relevant timestamp from a [`Hit`] given a [`TimeSource`].
fn hit_timestamp(hit: &Hit, source: TimeSource) -> Hlc {
    match source {
        // Both variants currently map to `valid_from` — `Hit` only carries
        // the valid-time stamp today. The `SysFrom` arm reserves the
        // surface for when hydrate threads sys-time through.
        TimeSource::ValidFrom | TimeSource::SysFrom => hit.valid_from,
    }
}

/// Compute the age (seconds, f32, clamped to non-negative) between `now` and
/// the hit's selected timestamp. Returns `None` when the hit carries no
/// recency signal (`valid_from == Hlc::ZERO`), so the rescorer can pass the
/// prior score through unchanged.
fn hit_age_seconds(hit: &Hit, now: Hlc, source: TimeSource) -> Option<f32> {
    let ts = hit_timestamp(hit, source);
    if ts == Hlc::ZERO {
        return None;
    }
    let delta_ms = now.wall_ms.saturating_sub(ts.wall_ms);
    Some((delta_ms as f32) / 1000.0)
}

/// Apply the recency rescorer in-place and re-sort descending.
///
/// Public entrypoint — call AFTER [`crate::hydrate::hydrate`] and BEFORE
/// returning hits to the caller. The free-function shape (rather than a
/// builder method) is intentional: P0 #2 (parallel agent) owns `builder.rs`
/// this round, so the recency wiring is a follow-up.
///
/// Guarantees:
///
/// - O(n) decay pass + O(n log n) sort.
/// - No allocation beyond the existing `hits` buffer.
/// - Stable under simultaneous timestamps (sort uses `partial_cmp` and the
///   pre-existing slot order acts as a tiebreaker via the stable sort).
/// - NaN scorer output is treated as "no signal" and falls back to the
///   prior score.
pub fn rescore_recency(hits: &mut [Hit], now: Hlc, config: &RecencyConfig) {
    if hits.is_empty() {
        return;
    }
    for hit in hits.iter_mut() {
        let new_score = match hit_age_seconds(hit, now, config.source) {
            Some(age) => {
                let candidate = config.scorer.decay(age, hit.score);
                if candidate.is_nan() { hit.score } else { candidate }
            }
            None => hit.score,
        };
        hit.score = new_score;
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{Hit, SourceOp};

    fn mk_hit(id: u8, score: f32, valid_from_ms: u64) -> Hit {
        Hit {
            id: vec![id],
            episode_id: Vec::new(),
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
        let mut hits = vec![mk_hit(99, 0.5, now_ms - thirty_days_ms), mk_hit(7, 0.5, now_ms)];
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
