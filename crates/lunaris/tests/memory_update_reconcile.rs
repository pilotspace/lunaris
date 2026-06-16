//! RED-first (memory-update-intelligence) — the cross-episode reconciliation
//! decision core. `lunaris::reconcile` does not exist yet → compile-fail RED.
//!
//! `classify_fact(new, prior)` is the pure heart of the feature: given a new
//! fact triple and the existing `(subject,predicate)` index entries in the
//! SAME scope, decide Noop (exact dup) / Append (new or non-overlapping) /
//! Supersede (different object, overlapping validity → the async-supersede
//! path closes the named loser). Green lands in build batch 3–4.

use chrono::{DateTime, TimeZone, Utc};
use lunaris::reconcile::{FactDecision, FactTriple, SpoEntry, classify_fact};
use lunaris_extract::types::EntityId;
use ulid::Ulid;

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).single().unwrap()
}

fn alice() -> EntityId {
    EntityId::from_name_and_type("Alice", "Person")
}
fn acme() -> EntityId {
    EntityId::from_name_and_type("Acme", "Org")
}
fn globex() -> EntityId {
    EntityId::from_name_and_type("Globex", "Org")
}

fn new_fact(object: EntityId, vf: DateTime<Utc>, vt: Option<DateTime<Utc>>) -> FactTriple {
    FactTriple {
        subject_id: alice(),
        predicate: "employer".to_string(),
        object_id: object,
        valid_from: vf,
        valid_to: vt,
    }
}

#[test]
fn exact_object_match_is_noop() {
    // Same (subject,predicate,object) → identical FactId → idempotent NOOP,
    // regardless of the validity window (the row is overwritten in place).
    let prior = vec![SpoEntry {
        object_id: acme(),
        fact_id: Ulid::new(),
        valid_from: day(2020, 1, 1),
        valid_to: None,
    }];
    let new = new_fact(acme(), day(2021, 6, 1), None);
    assert!(
        matches!(classify_fact(&new, &prior), FactDecision::Noop),
        "re-asserting the same triple must be a NOOP (dedup)"
    );
}

#[test]
fn different_object_overlapping_validity_is_supersede() {
    // (Alice, employer, Acme) [2020-, open) vs (Alice, employer, Globex)
    // [2023-, open) → overlapping windows, different object → contradiction →
    // Supersede naming the EXISTING fact as loser.
    let existing = Ulid::new();
    let prior = vec![SpoEntry {
        object_id: acme(),
        fact_id: existing,
        valid_from: day(2020, 1, 1),
        valid_to: None,
    }];
    let new = new_fact(globex(), day(2023, 1, 1), None);
    match classify_fact(&new, &prior) {
        FactDecision::Supersede { loser_fact_id } => {
            assert_eq!(loser_fact_id, existing, "the older overlapping fact is the loser");
        }
        other => panic!("expected Supersede, got {other:?}"),
    }
}

#[test]
fn different_object_disjoint_validity_is_append() {
    // Legitimate temporal succession: Acme [2020,2022) then Globex [2023,open)
    // → NON-overlapping → additive, NEVER a supersede ("no_false_supersede").
    let prior = vec![SpoEntry {
        object_id: acme(),
        fact_id: Ulid::new(),
        valid_from: day(2020, 1, 1),
        valid_to: Some(day(2022, 1, 1)),
    }];
    let new = new_fact(globex(), day(2023, 1, 1), None);
    assert!(
        matches!(classify_fact(&new, &prior), FactDecision::Append),
        "non-overlapping succession must stay additive (no false supersede)"
    );
}

#[test]
fn touching_intervals_do_not_overlap_is_append() {
    // Half-open boundary: Acme [2020, 2022) then Globex [2022, open). The new
    // window STARTS exactly when the prior one ENDS → they must NOT overlap
    // (half-open `[from, to)`) → additive, never a supersede.
    let prior = vec![SpoEntry {
        object_id: acme(),
        fact_id: Ulid::new(),
        valid_from: day(2020, 1, 1),
        valid_to: Some(day(2022, 1, 1)),
    }];
    let new = new_fact(globex(), day(2022, 1, 1), None);
    assert!(
        matches!(classify_fact(&new, &prior), FactDecision::Append),
        "touching half-open intervals must NOT overlap → additive"
    );
}

#[test]
fn no_prior_is_append() {
    let new = new_fact(acme(), day(2024, 1, 1), None);
    assert!(
        matches!(classify_fact(&new, &[]), FactDecision::Append),
        "a brand-new (subject,predicate) must be additive"
    );
}
