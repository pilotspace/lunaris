//! RED-first (memory-update-intelligence) — sync-dedup substrate + the
//! cross-episode contradiction reason.
//!
//! These reference symbols that do not exist yet (`FactId` / `FactId::from_triple`
//! in `lunaris_extract::types`, and `NeedsReviewReason::CrossEpisodeContradiction`),
//! so the test target fails to COMPILE: the right-reason RED for a Rust feature (the
//! analog of import-red). Green lands in build batches 1 & 5.

use lunaris_extract::types::{EntityId, FactId};
use lunaris_extract::validator::NeedsReviewReason;
use ulid::Ulid;

#[test]
fn factid_from_triple_is_deterministic_and_distinct() {
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let acme = EntityId::from_name_and_type("Acme", "Org");
    let globex = EntityId::from_name_and_type("Globex", "Org");

    // Same triple → same FactId (this is the dedup key).
    let a1 = FactId::from_triple(alice, "employer", acme);
    let a2 = FactId::from_triple(alice, "employer", acme);
    assert_eq!(a1, a2, "identical triple must yield the same FactId");

    // Different object → different FactId (a value change is NOT a dup).
    let diff_obj = FactId::from_triple(alice, "employer", globex);
    assert_ne!(a1, diff_obj, "different object must yield a different FactId");

    // Different predicate → different FactId.
    let diff_pred = FactId::from_triple(alice, "founder", acme);
    assert_ne!(a1, diff_pred, "different predicate must yield a different FactId");

    // 16-byte id so it can back a `fact_key` (Ulid-shaped).
    assert_eq!(a1.0.len(), 16, "FactId must be 16 bytes to key a fact row");
}

#[test]
fn cross_episode_contradiction_reason_carries_both_fact_ids() {
    let reason = NeedsReviewReason::CrossEpisodeContradiction {
        subject: EntityId::from_name_and_type("Alice", "Person"),
        predicate: "employer".to_string(),
        existing_fact_id: Ulid::new(),
        existing_object: EntityId::from_name_and_type("Acme", "Org"),
        new_fact_id: Ulid::new(),
        new_object: EntityId::from_name_and_type("Globex", "Org"),
    };
    // Must render (it derives `Error`/`Display` like the sibling variants) so
    // the verify worker can log + arbitrate it.
    let rendered = format!("{reason}");
    assert!(
        rendered.contains("employer"),
        "rendered reason should mention the predicate; got {rendered}"
    );
}
