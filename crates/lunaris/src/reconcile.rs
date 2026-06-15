//! Cross-episode memory-convergence reconciliation — the pure decision core.
//!
//! At structured ingest, every freshly-extracted fact is classified against the
//! existing in-scope rows that assert the SAME `(subject, predicate)` (read via
//! [`lunaris_core::keyspace::fact_spo_key`]). [`classify_fact`] is the pure heart
//! of the hybrid memory-update feature (`memory-update-intelligence`):
//!
//! - **[`FactDecision::Noop`]** — the new triple re-asserts an EXISTING
//!   `(subject, predicate, object)`. Its deterministic
//!   [`FactId`](lunaris_extract::types::FactId) collides with the prior row, so
//!   the write overwrites in place: no duplicate row accrues (sync dedup).
//! - **[`FactDecision::Supersede`]** — the new fact asserts a DIFFERENT object
//!   whose validity window OVERLAPS an existing one. This is a cross-episode
//!   contradiction; the named loser is routed through the async verify →
//!   `apply_supersede` path (latest-assertion-wins) to close its bi-temporal
//!   interval.
//! - **[`FactDecision::Append`]** — a brand-new `(subject, predicate)`, or a
//!   different object with a DISJOINT window (legitimate temporal succession,
//!   e.g. employer A `[2020, 2022)` then employer B `[2023, …)`). Additive,
//!   never a false supersede.
//!
//! Keeping the policy a pure function over `(new, prior[])` makes it testable
//! without storage or model weights; the structured-ingest write path supplies
//! `prior` from the spo-index read and acts on the returned decision.

use chrono::{DateTime, Utc};
use lunaris_extract::types::EntityId;
use ulid::Ulid;

/// A newly-extracted fact triple awaiting classification, with its asserted
/// validity window (half-open `[valid_from, valid_to)`; `valid_to == None`
/// means the assertion is still open / current).
#[derive(Clone, Debug, PartialEq)]
pub struct FactTriple {
    pub subject_id: EntityId,
    pub predicate: String,
    pub object_id: EntityId,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// One existing row from the `(subject, predicate)` spo-index — the prior facts
/// already stored in this scope for the same subject + predicate.
#[derive(Clone, Debug, PartialEq)]
pub struct SpoEntry {
    pub object_id: EntityId,
    pub fact_id: Ulid,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// The reconciliation verdict for a new fact against its prior spo-index rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FactDecision {
    /// Exact `(subject, predicate, object)` re-assertion → idempotent dedup,
    /// no new row (the deterministic `FactId` overwrites in place).
    Noop,
    /// New or non-overlapping assertion → additive write.
    Append,
    /// Different object with an OVERLAPPING window → cross-episode
    /// contradiction; `loser_fact_id` is the prior fact to be superseded
    /// (closed) by the async verifier.
    Supersede { loser_fact_id: Ulid },
}

/// Classify `new` against the existing `(subject, predicate)` rows `prior`.
///
/// Precedence:
/// 1. An EXACT object match (re-assertion) is a [`FactDecision::Noop`],
///    regardless of the validity window — dedup wins over everything.
/// 2. Otherwise the FIRST prior row with a DIFFERENT object whose window
///    overlaps the new one yields a [`FactDecision::Supersede`] naming that
///    row as the loser (latest-assertion-wins; if several priors overlap, the
///    earliest-seen is named — multi-overlap is itself a pre-existing
///    inconsistency the verifier resolves over successive passes).
/// 3. Otherwise (no prior, or only disjoint windows) it is additive:
///    [`FactDecision::Append`].
#[must_use]
pub fn classify_fact(new: &FactTriple, prior: &[SpoEntry]) -> FactDecision {
    // (1) Exact re-assertion → dedup NOOP, irrespective of the window.
    if prior.iter().any(|p| p.object_id == new.object_id) {
        return FactDecision::Noop;
    }

    // (2) Different object + overlapping validity → cross-episode contradiction.
    for p in prior {
        // (p.object_id != new.object_id holds — exact matches returned above.)
        if intervals_overlap(new.valid_from, new.valid_to, p.valid_from, p.valid_to) {
            return FactDecision::Supersede { loser_fact_id: p.fact_id };
        }
    }

    // (3) New subject/predicate, or only disjoint succession → additive.
    FactDecision::Append
}

/// Do two half-open intervals `[a_from, a_to)` and `[b_from, b_to)` overlap?
/// A `None` upper bound is treated as `+∞` (still-open assertion).
fn intervals_overlap(
    a_from: DateTime<Utc>,
    a_to: Option<DateTime<Utc>>,
    b_from: DateTime<Utc>,
    b_to: Option<DateTime<Utc>>,
) -> bool {
    // Overlap ⇔ a starts before b ends AND b starts before a ends.
    let a_before_b_end = b_to.is_none_or(|bt| a_from < bt);
    let b_before_a_end = a_to.is_none_or(|at| b_from < at);
    a_before_b_end && b_before_a_end
}
