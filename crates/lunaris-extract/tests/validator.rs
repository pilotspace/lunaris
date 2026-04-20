//! Episode-local Validator table-driven tests (D-08 + D-21 coverage).
//!
//! Covers all 4 [`NeedsReviewReason`] variants + happy path + episode-local
//! contradiction detection + sentinel sentinel routing + never-silently-
//! drops invariant.
//!
//! Per D-09 cross-Episode contradictions are NOT detected here — the
//! `cross_episode_NOT_detected` test pins this to the Phase 4 Verifier
//! handoff. Those tests document the contract boundary explicitly so
//! future plans don't accidentally widen the scope here.

use lunaris_extract::validator::{
    NeedsReviewItem, NeedsReviewReason, TRANSIENT_SENTINEL_NAME, TRANSIENT_SENTINEL_TYPE, validate,
};
use lunaris_extract::{Entity, EntityId, Fact, RawExtraction, RawExtractionBatch, Relation};
use ulid::Ulid;

// -------------------------- helpers ----------------------------------------

fn entity_alice() -> Entity {
    Entity {
        id: EntityId::from_name_and_type("Alice", "Person"),
        name: "Alice".into(),
        aliases: vec![],
        entity_type: "Person".into(),
        confidence: 0.9,
        valid_from_iso: "2024-01-01T00:00:00Z".into(),
        valid_to_iso: None,
    }
}

fn entity_paris() -> Entity {
    Entity {
        id: EntityId::from_name_and_type("Paris", "Place"),
        name: "Paris".into(),
        aliases: vec![],
        entity_type: "Place".into(),
        confidence: 0.95,
        valid_from_iso: "2024-01-01T00:00:00Z".into(),
        valid_to_iso: None,
    }
}

fn relation(
    subject: &str,
    subject_type: &str,
    predicate: &str,
    object: &str,
    object_type: &str,
    valid_from: &str,
    valid_to: Option<&str>,
) -> Relation {
    Relation {
        subject_id: EntityId::from_name_and_type(subject, subject_type),
        predicate: predicate.into(),
        object_id: EntityId::from_name_and_type(object, object_type),
        confidence: 0.9,
        valid_from_iso: valid_from.into(),
        valid_to_iso: valid_to.map(str::to_string),
    }
}

fn one_chunk(
    entities: Vec<Entity>,
    relations: Vec<Relation>,
    facts: Vec<Fact>,
) -> RawExtractionBatch {
    RawExtractionBatch {
        by_chunk: vec![RawExtraction { source_chunk_id: Ulid::new(), entities, relations, facts }],
    }
}

// ----------------------- happy path + smoke --------------------------------

#[test]
fn valid_extraction_passes_through() {
    let batch = one_chunk(vec![entity_alice(), entity_paris()], vec![], vec![]);
    let v = validate(batch);
    assert_eq!(v.entities.len(), 2);
    assert!(v.relations.is_empty());
    assert!(v.facts.is_empty());
    assert!(v.needs_review.is_empty(), "well-formed batch must produce 0 needs_review");
}

// ----------------------- (1) InvalidBitemporal -----------------------------

#[test]
fn invalid_bitemporal_flagged_on_entity() {
    // valid_from = 2025-01-01 >= valid_to = 2024-01-01 → flag.
    let bad = Entity {
        valid_from_iso: "2025-01-01T00:00:00Z".into(),
        valid_to_iso: Some("2024-01-01T00:00:00Z".into()),
        ..entity_alice()
    };
    let batch = one_chunk(vec![bad.clone()], vec![], vec![]);
    let v = validate(batch);
    assert!(v.entities.is_empty(), "invalid entity must NOT land in entities");
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Entity { reason, raw } => {
            assert!(matches!(reason, NeedsReviewReason::InvalidBitemporal { .. }));
            assert_eq!(raw, &bad);
        }
        other => panic!("expected Entity needs_review, got {other:?}"),
    }
}

#[test]
fn invalid_bitemporal_flagged_on_relation() {
    let bad = relation(
        "Alice",
        "Person",
        "lived_in",
        "Paris",
        "Place",
        "2025-01-01T00:00:00Z",
        Some("2020-01-01T00:00:00Z"),
    );
    let batch = one_chunk(vec![], vec![bad.clone()], vec![]);
    let v = validate(batch);
    assert!(v.relations.is_empty());
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Relation { reason, raw } => {
            assert!(matches!(reason, NeedsReviewReason::InvalidBitemporal { .. }));
            assert_eq!(raw, &bad);
        }
        other => panic!("expected Relation needs_review, got {other:?}"),
    }
}

#[test]
fn invalid_bitemporal_flagged_on_fact() {
    let bad = Fact {
        id: Ulid::new(),
        subject_id: EntityId::from_name_and_type("Alice", "Person"),
        predicate: "born_in".into(),
        object_id: EntityId::from_name_and_type("Paris", "Place"),
        fact_text: "Alice was born in Paris.".into(),
        confidence: 0.9,
        valid_from_iso: "2025-01-01T00:00:00Z".into(),
        valid_to_iso: Some("2024-01-01T00:00:00Z".into()),
    };
    let batch = one_chunk(vec![], vec![], vec![bad.clone()]);
    let v = validate(batch);
    assert!(v.facts.is_empty());
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Fact { reason, raw } => {
            assert!(matches!(reason, NeedsReviewReason::InvalidBitemporal { .. }));
            assert_eq!(raw, &bad);
        }
        other => panic!("expected Fact needs_review, got {other:?}"),
    }
}

// ----------------------- (3) GbnfFailure -----------------------------------

#[test]
fn gbnf_failure_on_empty_name() {
    let bad = Entity { name: "".into(), ..entity_alice() };
    let batch = one_chunk(vec![bad.clone()], vec![], vec![]);
    let v = validate(batch);
    assert!(v.entities.is_empty());
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Entity { reason, raw } => {
            match reason {
                NeedsReviewReason::GbnfFailure { schema_path, error } => {
                    assert_eq!(schema_path, "entities.gbnf");
                    assert!(error.contains("invalid name/type/confidence"), "got: {error}");
                }
                other => panic!("expected GbnfFailure, got {other:?}"),
            }
            assert_eq!(raw, &bad);
        }
        other => panic!("expected Entity needs_review, got {other:?}"),
    }
}

#[test]
fn gbnf_failure_on_out_of_range_confidence() {
    let bad =
        relation("Alice", "Person", "lived_in", "Paris", "Place", "2024-01-01T00:00:00Z", None);
    let bad = Relation { confidence: 1.5, ..bad };
    let batch = one_chunk(vec![], vec![bad.clone()], vec![]);
    let v = validate(batch);
    assert!(v.relations.is_empty(), "out-of-range confidence must NOT pass");
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Relation { reason, raw } => {
            match reason {
                NeedsReviewReason::GbnfFailure { schema_path, error } => {
                    assert_eq!(schema_path, "relations.gbnf");
                    assert!(error.contains("conf=1.5"), "got: {error}");
                }
                other => panic!("expected GbnfFailure, got {other:?}"),
            }
            assert_eq!(raw, &bad);
        }
        other => panic!("expected Relation needs_review, got {other:?}"),
    }
}

#[test]
fn gbnf_failure_on_empty_fact_text() {
    let bad = Fact {
        id: Ulid::new(),
        subject_id: EntityId::from_name_and_type("Alice", "Person"),
        predicate: "born_in".into(),
        object_id: EntityId::from_name_and_type("Paris", "Place"),
        fact_text: "".into(),
        confidence: 0.9,
        valid_from_iso: "2024-01-01T00:00:00Z".into(),
        valid_to_iso: None,
    };
    let batch = one_chunk(vec![], vec![], vec![bad.clone()]);
    let v = validate(batch);
    assert!(v.facts.is_empty());
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Fact { reason, raw } => {
            assert!(matches!(reason, NeedsReviewReason::GbnfFailure { .. }));
            assert_eq!(raw, &bad);
        }
        other => panic!("expected Fact needs_review, got {other:?}"),
    }
}

// ----------------------- (2) StructuralContradiction -----------------------

#[test]
fn structural_contradiction_demotes_overlapping_relations() {
    // (Alice, born_in, Paris, [2020, 2030]) AND (Alice, born_in, London, [2020, 2030])
    // → BOTH go to needs_review with StructuralContradiction.
    // NEITHER lands in out.relations because we can't pick a winner without
    // the Phase 4 Verifier (D-09).
    let r1 = relation(
        "Alice",
        "Person",
        "born_in",
        "Paris",
        "Place",
        "2020-01-01T00:00:00Z",
        Some("2030-01-01T00:00:00Z"),
    );
    let r2 = relation(
        "Alice",
        "Person",
        "born_in",
        "London",
        "Place",
        "2020-01-01T00:00:00Z",
        Some("2030-01-01T00:00:00Z"),
    );
    let batch = one_chunk(vec![], vec![r1.clone(), r2.clone()], vec![]);
    let v = validate(batch);
    assert!(
        v.relations.is_empty(),
        "overlapping conflicting relations must NEVER land in out.relations (got {:?})",
        v.relations
    );
    assert_eq!(v.needs_review.len(), 2, "BOTH conflicting relations must be demoted");
    let mut found_paris = false;
    let mut found_london = false;
    for item in &v.needs_review {
        match item {
            NeedsReviewItem::Relation { reason, raw } => {
                match reason {
                    NeedsReviewReason::StructuralContradiction { subject, predicate, conflict } => {
                        assert_eq!(*subject, EntityId::from_name_and_type("Alice", "Person"));
                        assert_eq!(predicate, "born_in");
                        assert_eq!(conflict.len(), 2);
                        assert!(conflict.contains(&EntityId::from_name_and_type("Paris", "Place")));
                        assert!(
                            conflict.contains(&EntityId::from_name_and_type("London", "Place"))
                        );
                    }
                    other => panic!("expected StructuralContradiction, got {other:?}"),
                }
                if raw.object_id == EntityId::from_name_and_type("Paris", "Place") {
                    found_paris = true;
                } else if raw.object_id == EntityId::from_name_and_type("London", "Place") {
                    found_london = true;
                }
            }
            other => panic!("expected Relation needs_review, got {other:?}"),
        }
    }
    assert!(found_paris && found_london, "BOTH conflicting relations must appear in needs_review");
}

#[test]
fn non_overlapping_relations_both_pass() {
    // (Alice, lived_in, Paris, [2010, 2015]) AND (Alice, lived_in, London, [2015, 2020])
    // → no overlap → no contradiction → both pass.
    let r1 = relation(
        "Alice",
        "Person",
        "lived_in",
        "Paris",
        "Place",
        "2010-01-01T00:00:00Z",
        Some("2015-01-01T00:00:00Z"),
    );
    let r2 = relation(
        "Alice",
        "Person",
        "lived_in",
        "London",
        "Place",
        "2015-01-01T00:00:00Z",
        Some("2020-01-01T00:00:00Z"),
    );
    let batch = one_chunk(vec![], vec![r1.clone(), r2.clone()], vec![]);
    let v = validate(batch);
    assert_eq!(v.relations.len(), 2, "non-overlapping intervals must both pass");
    assert!(v.needs_review.is_empty());
}

#[test]
fn same_object_repeated_does_not_trigger_contradiction() {
    // Two relations with same subject+predicate+object are RESTATING, not
    // contradicting — the bucket dedup logic must skip them.
    let r1 = relation(
        "Alice",
        "Person",
        "lived_in",
        "Paris",
        "Place",
        "2020-01-01T00:00:00Z",
        Some("2030-01-01T00:00:00Z"),
    );
    let r2 = relation(
        "Alice",
        "Person",
        "lived_in",
        "Paris",
        "Place",
        "2025-01-01T00:00:00Z",
        Some("2035-01-01T00:00:00Z"),
    );
    let batch = one_chunk(vec![], vec![r1, r2], vec![]);
    let v = validate(batch);
    assert_eq!(v.relations.len(), 2, "same-object repetition is restating, not contradicting");
    assert!(v.needs_review.is_empty());
}

// ----------------------- (4) TransientAfterRetry sentinel -------------------

#[test]
fn transient_after_retry_sentinel_routed_to_needs_review() {
    // W-1 fix: cloud_api emits a sentinel Entity with reserved entity_type
    // and the wrapped error in valid_from_iso. Validator detects it BEFORE
    // any other check and routes to TransientAfterRetry.
    let sentinel = Entity {
        id: EntityId::from_name_and_type(TRANSIENT_SENTINEL_NAME, TRANSIENT_SENTINEL_TYPE),
        name: TRANSIENT_SENTINEL_NAME.into(),
        aliases: vec![],
        entity_type: TRANSIENT_SENTINEL_TYPE.into(),
        confidence: 0.0,
        valid_from_iso: "transient: HTTP 503".into(),
        valid_to_iso: None,
    };
    let batch = one_chunk(vec![sentinel.clone()], vec![], vec![]);
    let v = validate(batch);
    assert!(v.entities.is_empty(), "sentinel must NEVER land in out.entities");
    assert_eq!(v.needs_review.len(), 1);
    match &v.needs_review[0] {
        NeedsReviewItem::Entity { reason, raw } => {
            match reason {
                NeedsReviewReason::TransientAfterRetry(msg) => {
                    assert_eq!(msg, "HTTP 503", "wrapped error text must be extracted");
                }
                other => panic!("expected TransientAfterRetry, got {other:?}"),
            }
            assert_eq!(raw, &sentinel);
        }
        other => panic!("expected Entity needs_review, got {other:?}"),
    }
}

// ----------------------- D-09 boundary: cross-Episode --------------------

#[test]
#[allow(non_snake_case)]
fn cross_episode_NOT_detected_by_validator() {
    // The validator IS Episode-local but works across all chunks in a
    // RawExtractionBatch. Cross-Episode contradiction (e.g., Episode A says
    // Alice born 1990, Episode B says 1991) is D-09 deferred to the Phase 4
    // Verifier — there is NO call site where the validator sees two
    // Episodes' batches together.
    //
    // This test pins the contract: two NON-OVERLAPPING relations in the SAME
    // batch (which the validator DOES treat as a single Episode) both pass
    // because the intervals don't overlap. The same data spread across two
    // separate `validate()` calls (representing two Episodes) trivially
    // can't contradict because the validator only sees one batch at a time.
    let r1 = relation(
        "Alice",
        "Person",
        "born_in",
        "Paris",
        "Place",
        "2020-01-01T00:00:00Z",
        Some("2024-01-01T00:00:00Z"),
    );
    let r2 = relation(
        "Alice",
        "Person",
        "born_in",
        "London",
        "Place",
        "2024-01-01T00:00:00Z",
        Some("2030-01-01T00:00:00Z"),
    );
    // SAME batch, NON-overlapping intervals → both pass.
    let same_batch = one_chunk(vec![], vec![r1.clone(), r2.clone()], vec![]);
    let v = validate(same_batch);
    assert_eq!(v.relations.len(), 2);
    assert!(v.needs_review.is_empty());

    // Trivially, two separate `validate()` calls each pass independently —
    // the validator never compares across calls.
    let batch_a = one_chunk(vec![], vec![r1], vec![]);
    let batch_b = one_chunk(vec![], vec![r2], vec![]);
    assert_eq!(validate(batch_a).relations.len(), 1);
    assert_eq!(validate(batch_b).relations.len(), 1);
}

// ----------------------- never silently dropped invariant -------------------

#[test]
fn needs_review_never_silently_dropped() {
    // EXTRACT-05 contract: every invalid item lands in needs_review with a
    // structured reason. NOTHING gets silently dropped. We construct a batch
    // with one invalid item per category and assert
    // out.needs_review.len() == invalid_count AND
    // out.{entities,relations,facts} are all empty.
    let bad_entity = Entity { name: "".into(), ..entity_alice() };
    let bad_relation = Relation {
        confidence: 1.5,
        ..relation("Alice", "Person", "lived_in", "Paris", "Place", "2024-01-01T00:00:00Z", None)
    };
    let bad_fact = Fact {
        id: Ulid::new(),
        subject_id: EntityId::from_name_and_type("Alice", "Person"),
        predicate: "born_in".into(),
        object_id: EntityId::from_name_and_type("Paris", "Place"),
        fact_text: "Alice was born in Paris.".into(),
        confidence: 0.9,
        valid_from_iso: "2025-01-01T00:00:00Z".into(),
        valid_to_iso: Some("2020-01-01T00:00:00Z".into()), // valid_from >= valid_to
    };
    let sentinel = Entity {
        id: EntityId::from_name_and_type(TRANSIENT_SENTINEL_NAME, TRANSIENT_SENTINEL_TYPE),
        name: TRANSIENT_SENTINEL_NAME.into(),
        aliases: vec![],
        entity_type: TRANSIENT_SENTINEL_TYPE.into(),
        confidence: 0.0,
        valid_from_iso: "transient: timeout".into(),
        valid_to_iso: None,
    };
    let batch = RawExtractionBatch {
        by_chunk: vec![RawExtraction {
            source_chunk_id: Ulid::new(),
            entities: vec![bad_entity, sentinel],
            relations: vec![bad_relation],
            facts: vec![bad_fact],
        }],
    };
    let total_invalid = 4_usize;
    let v = validate(batch);
    assert_eq!(v.needs_review.len(), total_invalid, "every invalid item must land in needs_review");
    assert!(v.entities.is_empty(), "no invalid entity should land in out.entities");
    assert!(v.relations.is_empty(), "no invalid relation should land in out.relations");
    assert!(v.facts.is_empty(), "no invalid fact should land in out.facts");

    // Confirm one of each reason is represented.
    let has_bitemporal = v
        .needs_review
        .iter()
        .any(|i| matches!(reason_of(i), NeedsReviewReason::InvalidBitemporal { .. }));
    let has_gbnf = v
        .needs_review
        .iter()
        .any(|i| matches!(reason_of(i), NeedsReviewReason::GbnfFailure { .. }));
    let has_transient = v
        .needs_review
        .iter()
        .any(|i| matches!(reason_of(i), NeedsReviewReason::TransientAfterRetry(_)));
    assert!(has_bitemporal, "must include InvalidBitemporal");
    assert!(has_gbnf, "must include GbnfFailure");
    assert!(has_transient, "must include TransientAfterRetry");
}

fn reason_of(item: &NeedsReviewItem) -> &NeedsReviewReason {
    match item {
        NeedsReviewItem::Entity { reason, .. } => reason,
        NeedsReviewItem::Relation { reason, .. } => reason,
        NeedsReviewItem::Fact { reason, .. } => reason,
    }
}
