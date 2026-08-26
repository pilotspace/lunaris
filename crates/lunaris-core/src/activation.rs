//! ADD task activation-ledger — the persistent per-memory activation ledger.
//!
//! One [`ActivationRecord`] per `(scope, memory-ulid)` at
//! [`crate::keyspace::activation_key`] holds an O(1) running summary — total
//! refs, a Petrov-2006-style weighted running sum, and the first/last
//! reference wall times — so a memory's usage-graded recall prior can be
//! recomputed at READ time (Anderson 1996 base-level activation) without
//! walking a per-reference history log. See `.add/tasks/activation-ledger/TASK.md`
//! §3 CONTRACT (frozen) for the full shape.
//!
//! Layering: this module lives in `lunaris-core` (RC-1 — keyspace + shared
//! data types belong here, never in a caller crate) so the write side
//! (`lunaris::ScopedLunaris::record_activation_refs`), the read side
//! (`lunaris_retrieve::LedgerBoostProvider`), and the consolidation reader
//! (`lunaris_consolidate::LedgerReferenceSource`) all depend on ONE
//! definition of the record shape and its math.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Reference granularity — the unit of work the signal came from. `Node` is
/// reserved for the procedural-memory follow-on (2026-07-17 grain
/// amendment) and is not emitted by any writer in this task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Grain {
    #[default]
    Turn,
    ToolCall,
    Node,
}

/// Reference strength — the writer's DECLARATION of how strong a usage
/// signal is. The ledger stores it verbatim; it does not judge (§1 Must).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    #[default]
    Weak,
    Strong,
    /// engram-soul-loop task 4 — an explicit NEGATIVE human/agent vote
    /// (`memory.feedback` with `sentiment: negative`). Declared as its own
    /// variant (not a negated `Strong`) so the ledger's `last_strength`
    /// field records the writer's DECLARATION verbatim, same as every other
    /// variant (§1 Must — "the ledger stores it verbatim; it does not
    /// judge").
    StrongNegative,
}

/// Weight contributed by a `Strength::Weak` signal (e.g. a prompt-context
/// injection) to `ActivationRecord::weighted`.
pub const WEIGHT_WEAK: f64 = 1.0;
/// Weight contributed by a `Strength::Strong` signal (e.g. a citation,
/// positive feedback, or a successful tool call fed from the memory) to
/// `ActivationRecord::weighted`.
pub const WEIGHT_STRONG: f64 = 3.0;
/// Weight contributed by a `Strength::StrongNegative` signal (explicit
/// negative `memory.feedback`) to `ActivationRecord::weighted`. Negative by
/// design — `ActivationRecord::apply` floors the running sum at `0.0` so a
/// downvote can never push a memory's activation below the neutral
/// baseline, only toward it.
pub const WEIGHT_STRONG_NEGATIVE: f64 = -3.0;

/// Midpoint of the boost curve, in ACT-R activation units: an activation of
/// exactly `BOOST_MIDPOINT` earns half of [`BOOST_CAP`]. Zero is the natural
/// choice — it is the point where a record's weighted reference sum exactly
/// offsets its decay (`weighted == elapsed^decay`).
pub const BOOST_MIDPOINT: f64 = 0.0;

/// Width of the boost curve, in ACT-R activation units (a natural-log scale,
/// so `3.0` spans roughly a 20x swing in `weighted * elapsed^-decay` per
/// unit). Larger = flatter; this is the tuning dial, and retuning it is
/// benchmark-gated (F43).
pub const BOOST_SCALE: f64 = 3.0;

/// F43 — RETIRED. Was the slope `k` in the old `k * ln(1 + activation)`
/// curve, which clamped negative activation to zero and so produced exactly
/// 0.0 for every memory older than `weighted^2` seconds. Kept as a public
/// constant so the crate's surface does not shrink; nothing reads it.
#[deprecated(note = "F43: superseded by BOOST_MIDPOINT / BOOST_SCALE; the \
                     k * ln(1 + a) curve made the prior inert in production")]
pub const BOOST_K: f32 = 0.1;
/// Hard cap on the boost prior added to a hit's score, regardless of how
/// large the underlying activation grows.
pub const BOOST_CAP: f32 = 0.30;
/// Default Anderson-1996 decay parameter (`d`) used when recomputing
/// activation at read time. Matches `lunaris_consolidate::ConsolidatorConfig`'s
/// default `decay` — intentionally duplicated rather than shared, since
/// `lunaris-core` must not depend on `lunaris-consolidate` (layering).
pub const DEFAULT_DECAY: f64 = 0.5;

/// One usage signal for a memory id, as declared by the writer.
///
/// `grain` / `strength` are closed enums (not free strings) so a malformed
/// wire payload fails to deserialize rather than silently coercing (Reject
/// scenario 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefSignal {
    pub id: Ulid,
    pub grain: Grain,
    pub strength: Strength,
}

/// The persistent O(1) activation summary stored at
/// [`crate::keyspace::activation_key`]. `#[serde(deny_unknown_fields)]` per
/// the HTTP-DTO-discipline convention — a stored record is a strict schema,
/// not caller-extensible wire data.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationRecord {
    /// Total reference count (unweighted).
    pub n: u32,
    /// Weighted running sum: Σ weight(strength) across every reference
    /// applied to this record (Petrov 2006 O(1) accumulator — no per-ref
    /// history is retained).
    pub weighted: f64,
    /// Unix seconds of the FIRST reference ever applied. Immutable after
    /// the first `apply()`.
    pub first_ref_wall: u64,
    /// Unix seconds of the MOST RECENT reference. Advances on every `apply()`.
    pub last_ref_wall: u64,
    /// Grain of the most recent reference.
    pub last_grain: Grain,
    /// Strength of the most recent reference.
    pub last_strength: Strength,
    /// Schema version. `1` for this task.
    pub v: u8,
    /// engram-soul-loop task 8b (`memory.distill`) — unix-seconds this
    /// record was ARCHIVED (activation drop), or `None` while live.
    ///
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` is
    /// load-bearing twice over: `default` lets a pre-8b record (no
    /// `archived_at` key at all) still decode under this struct's
    /// `#[serde(deny_unknown_fields)]`; `skip_serializing_if` keeps an
    /// unarchived record's serialized bytes BYTE-IDENTICAL to the pre-8b
    /// shape (no new key ever appears on the wire until the record is
    /// actually archived). Archive is activation drop, NOT a tombstone —
    /// see [`Self::is_archived`] and `lunaris_retrieve::LedgerBoostProvider`
    /// (0 boost) / `lunaris_consolidate::dream::build_dream_agenda`
    /// (dropped from candidates). The episode itself is untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
}

impl Default for ActivationRecord {
    fn default() -> Self {
        Self {
            n: 0,
            weighted: 0.0,
            first_ref_wall: 0,
            last_ref_wall: 0,
            last_grain: Grain::default(),
            last_strength: Strength::default(),
            v: 1,
            archived_at: None,
        }
    }
}

impl ActivationRecord {
    /// Upsert math: apply one reference signal at unix-seconds `now`.
    ///
    /// - `n` increments by 1.
    /// - `weighted` accumulates `weight(s.strength)` (weak=1.0, strong=3.0,
    ///   strong_negative=-3.0), then FLOORS at `0.0` — a run of negative
    ///   feedback can drive a memory's ledger weight to neutral but never
    ///   negative (§3 CONTRACT: `weighted = (weighted + weight_for(s)).max(0.0)`).
    /// - `first_ref_wall` is set to `now` ONLY on the first-ever apply
    ///   (`n == 0` before this call) and never changes afterward.
    /// - `last_ref_wall` / `last_grain` / `last_strength` always take the
    ///   newest signal's values.
    pub fn apply(&mut self, s: &RefSignal, now: u64) {
        if self.n == 0 {
            self.first_ref_wall = now;
        }
        self.n = self.n.saturating_add(1);
        self.weighted = (self.weighted + weight_for(s.strength)).max(0.0);
        self.last_ref_wall = now;
        self.last_grain = s.grain;
        self.last_strength = s.strength;
    }

    /// Recompute activation at read time from the O(1) summary — an
    /// Anderson-1996 / Petrov-2006-style optimized-learning approximation:
    /// `ln(weighted * elapsed^(-decay))`, where `elapsed` is the wall-clock
    /// age (in seconds, floored at 1 to avoid `0^-d = ∞`) since the LAST
    /// reference.
    ///
    /// Exactness is not the contract (per the frozen §1 assumption) —
    /// ordering is: strictly monotonic increasing in `weighted` (more/
    /// stronger refs score higher), strictly monotonic decreasing as
    /// `elapsed` grows (older references decay), and — via
    /// [`boost_prior`] — cap-safe regardless of magnitude.
    pub fn activation(&self, now: u64, decay: f64) -> f64 {
        let elapsed = (now.saturating_sub(self.last_ref_wall)).max(1) as f64;
        let sum = self.weighted * elapsed.powf(-decay);
        sum.ln()
    }

    /// engram-soul-loop task 8b — `true` once `memory.distill` has archived
    /// this record (activation drop). Archived records contribute 0 boost
    /// (`lunaris_retrieve::LedgerBoostProvider::priors`) and are excluded
    /// from `memory.dream_agenda` candidates
    /// (`lunaris_consolidate::dream::build_dream_agenda`) — the underlying
    /// episode stays recall-hydratable; only its usage boost is suppressed.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

fn weight_for(s: Strength) -> f64 {
    match s {
        Strength::Weak => WEIGHT_WEAK,
        Strength::Strong => WEIGHT_STRONG,
        Strength::StrongNegative => WEIGHT_STRONG_NEGATIVE,
    }
}

/// Convert a raw recomputed `activation` value into the additive post-hydrate
/// boost prior: `BOOST_CAP * logistic((activation - BOOST_MIDPOINT) / BOOST_SCALE)`.
///
/// The result is always in `[0.0, BOOST_CAP]`, strictly increasing in
/// `activation`, and — this is the part that matters — non-zero across the
/// whole range a real memory occupies.
///
/// **F43.** This used to be `min(BOOST_CAP, BOOST_K * ln(1 + max(activation, 0)))`.
/// In ACT-R, base-level activation is normally NEGATIVE (the retrieval
/// threshold is negative too), so clamping negatives to zero collapsed the
/// entire operating range onto a single value. `activation` is
/// `ln(weighted * elapsed^-decay)` with `elapsed` in SECONDS, which turns
/// negative the moment `elapsed > weighted^decay_recip` — a cliff at
/// `weighted^2` seconds for the default decay of 0.5. Ten strong references
/// bought 15 minutes of prior and nothing afterward; `BOOST_CAP` required a
/// weighted sum of 1.944e8 and was never approached. Measured before and
/// after, ten strong refs (`weighted = 30`):
///
/// | age   |    10s |   1min |  10min |     1h |     6h |     1d |     7d |
/// |-------|--------|--------|--------|--------|--------|--------|--------|
/// | old   | 0.1179 | 0.0856 | 0.0185 | 0.0000 | 0.0000 | 0.0000 | 0.0000 |
/// | new   | 0.2038 | 0.1833 | 0.1551 | 0.1327 | 0.1112 | 0.0955 | 0.0758 |
///
/// A logistic is the minimal shape that fixes it: it is defined and monotone
/// over all of `(-inf, +inf)`, so no clamp is needed and no operating range
/// is privileged. The two boundaries are exact rather than approximate —
/// `-inf` maps to exactly `0.0` and `+inf` to exactly `BOOST_CAP` — which is
/// what keeps a NEVER-referenced memory (`weighted == 0.0`, so
/// `activation == -inf`) scoring exactly zero. That invariant is why the
/// defect could not be fixed by simply lifting the floor.
///
/// `BOOST_CAP` remains an ASYMPTOTE, not a reachable value, so the ceiling
/// this curve shares with the old one is a true bound in both.
pub fn boost_prior(activation: f64) -> f32 {
    // A corrupt ledger record must never poison the ranking. The old
    // `.max(0.0)` swallowed NaN by accident — Rust's `f64::max` returns the
    // non-NaN operand — so this now happens on purpose.
    if activation.is_nan() {
        return 0.0;
    }
    let z = (activation - BOOST_MIDPOINT) / BOOST_SCALE;
    let logistic = 1.0 / (1.0 + (-z).exp());
    ((BOOST_CAP as f64) * logistic).clamp(0.0, BOOST_CAP as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_record_has_zeroed_fields() {
        let r = ActivationRecord::default();
        assert_eq!(r.n, 0);
        assert_eq!(r.weighted, 0.0);
        assert_eq!(r.v, 1);
    }

    /// F43 — CONTRACT CORRECTION, not a weakened check. This test used to
    /// assert `boost_prior(-5.0) == 0.0` and `boost_prior(0.0) == 0.0`, which
    /// is precisely the behaviour that made the prior inert: negative
    /// activation is the NORMAL ACT-R range, not an error state, and a
    /// weighted sum that exactly offsets its own decay (activation 0.0) is a
    /// mid-strength memory rather than a worthless one. Zero is now reserved
    /// for the one case that means it: never referenced at all.
    #[test]
    fn boost_prior_reserves_zero_for_never_referenced() {
        assert_eq!(boost_prior(f64::NEG_INFINITY), 0.0, "never referenced");
        assert!(boost_prior(-5.0) > 0.0, "a stale but real memory is not nothing");
        assert_eq!(
            boost_prior(BOOST_MIDPOINT),
            BOOST_CAP / 2.0,
            "the midpoint earns exactly half the cap"
        );
        assert!(boost_prior(-5.0) < boost_prior(BOOST_MIDPOINT), "strictly increasing");
    }

    // ── F43 — the activation prior is inert in production ──────────────
    //
    // `activation()` is `ln(weighted * elapsed^-decay)` with `elapsed` in
    // SECONDS, and `boost_prior` clamps negatives to zero. So the prior
    // falls off a CLIFF at `elapsed == weighted^2` seconds and is exactly
    // 0.0 forever after: 9s for one strong ref, 15 min for ten. Every test
    // below is written against a memory with a REAL usage history at a
    // REALISTIC age — the regime the ledger exists to serve.

    /// A ladder of ages for one fixed usage history. Returns the realized
    /// prior at each age, oldest last.
    fn prior_ladder(weighted: f64) -> Vec<(u64, f32)> {
        [10u64, 60, 600, 3_600, 21_600, 86_400, 604_800]
            .iter()
            .map(|&secs| {
                let r = ActivationRecord {
                    n: 1,
                    weighted,
                    first_ref_wall: 0,
                    last_ref_wall: 0,
                    last_grain: Grain::default(),
                    last_strength: Strength::default(),
                    v: 1,
                    archived_at: None,
                };
                (secs, boost_prior(r.activation(secs, DEFAULT_DECAY)))
            })
            .collect()
    }

    /// The headline defect. Ten strong references an hour ago is a
    /// well-used memory by any reading, and it earns exactly nothing.
    #[test]
    fn f43_a_well_used_memory_still_earns_a_prior_after_an_hour() {
        let ladder = prior_ladder(30.0); // 10 strong refs
        let (_, at_one_hour) = ladder[3];
        assert!(
            at_one_hour > 0.0,
            "10 strong refs one hour old must still carry a prior; ladder = {ladder:?}"
        );
    }

    /// The ledger's whole purpose is to rank by usage. Today it cannot tell
    /// 50 references from 1 at any age past the cliff — both are 0.0, which
    /// is also what a NEVER-referenced memory scores.
    #[test]
    fn f43_usage_history_is_discriminating_at_realistic_ages() {
        for secs in [3_600u64, 86_400, 604_800] {
            let heavy = prior_ladder(150.0);
            let light = prior_ladder(3.0);
            let h = heavy.iter().find(|(s, _)| *s == secs).unwrap().1;
            let l = light.iter().find(|(s, _)| *s == secs).unwrap().1;
            assert!(
                h > l,
                "at {secs}s: 50 strong refs ({h}) must outrank 1 strong ref ({l})"
            );
        }
    }

    /// Decay must be a curve, not a cliff: strictly monotone decreasing
    /// across the whole realistic range, with no run of tied zeros.
    #[test]
    fn f43_prior_decays_strictly_and_never_flatlines_to_zero() {
        let ladder = prior_ladder(30.0);
        for w in ladder.windows(2) {
            let ((s0, p0), (s1, p1)) = (w[0], w[1]);
            assert!(
                p1 < p0,
                "prior must strictly decrease {s0}s -> {s1}s, got {p0} -> {p1}; ladder = {ladder:?}"
            );
            assert!(p1 > 0.0, "prior flatlined to zero at {s1}s; ladder = {ladder:?}");
        }
    }

    /// The invariant the fix must NOT break, and the reason this cannot be
    /// fixed by adding a constant: a memory nobody ever referenced has
    /// `weighted == 0.0`, so `activation` is `-inf` and the prior must be
    /// EXACTLY zero. A fix that lifts the floor for everyone fails here.
    #[test]
    fn f43_a_never_referenced_memory_earns_exactly_zero() {
        let ladder = prior_ladder(0.0);
        for (secs, p) in ladder {
            assert_eq!(p, 0.0, "never-referenced memory got {p} at {secs}s");
        }
    }

    /// A corrupt record must not poison the ranking with NaN. The old
    /// `.max(0.0)` swallowed this by accident (Rust's `f64::max` returns the
    /// non-NaN operand); any replacement has to handle it on purpose.
    #[test]
    fn f43_a_nan_activation_yields_zero_not_nan() {
        let p = boost_prior(f64::NAN);
        assert!(p.is_finite(), "NaN activation produced {p}");
        assert_eq!(p, 0.0);
    }

    #[test]
    fn boost_prior_never_exceeds_cap() {
        for a in [0.1, 1.0, 10.0, 1_000.0, 1_000_000.0] {
            assert!(boost_prior(a) <= BOOST_CAP);
            assert!(boost_prior(a) >= 0.0);
        }
    }

    // ── engram-soul-loop task 4 — memory.feedback: Strength::StrongNegative ──

    /// §3 CONTRACT: `weighted = (weighted + weight_for(s)).max(0.0)`. A weak
    /// ref (1.0) followed by a strong-negative ref (-3.0) must floor at 0.0,
    /// never go negative — and `last_strength` must serde round-trip through
    /// the new `"strong_negative"` wire value.
    #[test]
    fn strong_negative_weight_and_floor() {
        let mut r = ActivationRecord::default();
        r.apply(&RefSignal { id: Ulid::nil(), grain: Grain::Turn, strength: Strength::Weak }, 100);
        assert_eq!(r.weighted, WEIGHT_WEAK);

        r.apply(
            &RefSignal { id: Ulid::nil(), grain: Grain::Turn, strength: Strength::StrongNegative },
            200,
        );
        assert_eq!(r.weighted, 0.0, "1.0 + (-3.0) must floor at 0.0, not go negative");
        assert_eq!(r.n, 2);
        assert_eq!(r.last_strength, Strength::StrongNegative);
        assert_eq!(WEIGHT_STRONG_NEGATIVE, -3.0);

        let json = serde_json::to_string(&r.last_strength).unwrap();
        assert_eq!(json, "\"strong_negative\"");
        let back: Strength = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Strength::StrongNegative);
    }

    /// A record persisted before `StrongNegative` existed (last_strength
    /// `"weak"` / `"strong"` only) must still decode cleanly now that a
    /// third variant exists — adding a variant must never break old data at
    /// rest (§1 assumption).
    #[test]
    fn old_activation_record_without_strong_negative_still_decodes() {
        let raw = r#"{"n":1,"weighted":3.0,"first_ref_wall":1,"last_ref_wall":1,"last_grain":"turn","last_strength":"strong","v":1}"#;
        let record: ActivationRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(record.last_strength, Strength::Strong);
        assert_eq!(record.weighted, 3.0);
    }

    /// Existing Weak/Strong apply math stays byte-identical after the new
    /// variant lands — no floor kicks in when the sum never goes negative.
    #[test]
    fn weak_and_strong_apply_math_unchanged() {
        let mut r = ActivationRecord::default();
        r.apply(&RefSignal { id: Ulid::nil(), grain: Grain::Turn, strength: Strength::Weak }, 1);
        assert_eq!(r.weighted, 1.0);
        r.apply(&RefSignal { id: Ulid::nil(), grain: Grain::Turn, strength: Strength::Strong }, 2);
        assert_eq!(r.weighted, 4.0);
    }

    // ── engram-soul-loop task 8b — memory.distill: ActivationRecord::archived_at ──

    /// §6 VERIFY: "archived_at is serde-back-compat: an old ActivationRecord
    /// json (no field) decodes". A record persisted before task 8b (no
    /// `archived_at` key at all) must still decode under
    /// `#[serde(deny_unknown_fields)]`, and `is_archived()` must read `false`
    /// — the absence of the marker means "live".
    #[test]
    fn old_activation_record_without_archived_at_still_decodes() {
        let raw = r#"{"n":1,"weighted":3.0,"first_ref_wall":1,"last_ref_wall":1,"last_grain":"turn","last_strength":"strong","v":1}"#;
        let record: ActivationRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(record.archived_at, None);
        assert!(!record.is_archived());
    }

    /// An unarchived (default) record serializes WITHOUT the `archived_at`
    /// key at all (`skip_serializing_if`) — the on-disk bytes for the common
    /// case stay byte-identical to the pre-8b shape. Once archived, the key
    /// appears and round-trips.
    #[test]
    fn archived_at_skip_serializing_when_none_and_round_trips_when_set() {
        let live = ActivationRecord::default();
        assert!(!live.is_archived());
        let live_json = serde_json::to_string(&live).unwrap();
        assert!(
            !live_json.contains("archived_at"),
            "unarchived record must not emit archived_at: {live_json}"
        );

        let archived =
            ActivationRecord { archived_at: Some(1_700_000_000), ..ActivationRecord::default() };
        assert!(archived.is_archived());
        let archived_json = serde_json::to_string(&archived).unwrap();
        assert!(
            archived_json.contains("\"archived_at\":1700000000"),
            "archived record must serialize archived_at: {archived_json}"
        );
        let back: ActivationRecord = serde_json::from_str(&archived_json).unwrap();
        assert_eq!(back.archived_at, Some(1_700_000_000));
        assert!(back.is_archived());
    }

    /// `is_archived()` is a pure function of `archived_at` — true/false cases.
    #[test]
    fn is_archived_true_false() {
        let mut r = ActivationRecord::default();
        assert!(!r.is_archived());
        r.archived_at = Some(42);
        assert!(r.is_archived());
        r.archived_at = None;
        assert!(!r.is_archived());
    }
}
