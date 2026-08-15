//! KG-RAG wiring Wave D (2026-07-21): duplicate-triple canonicalization in
//! the validator.
//!
//! SOTA graph-RAG denoising (research synthesis) ranks dedup as the
//! highest-leverage extraction-side lever: the same assertion extracted from
//! adjacent chunks (or re-stated across a session) otherwise lands as N fact
//! rows + N vector entries and crowds the RRF fusion window with copies.
//!
//! Contract: within ONE `validate()` call, facts (and relations) that agree
//! on the FULL identity key — (subject_id, predicate, object_id,
//! valid_from_iso, valid_to_iso) — collapse to a single survivor carrying the
//! MAX confidence, first-seen position. Anything differing in ANY key
//! component (esp. the validity interval — distinct temporal assertions) is
//! preserved. This is canonicalization of redundant restatements, NOT
//! validation rejection — the EXTRACT-05 "nothing silently dropped to
//! needs_review-limbo" contract is untouched (duplicates are not invalid
//! items and the surviving row keeps the strongest confidence).
//!
//! RED until validator.rs adds the dedup pass.

use lunaris_extract::validator::validate;
use lunaris_extract::{EntityId, Fact, RawExtraction, RawExtractionBatch, Relation};
use ulid::Ulid;

fn fact(predicate: &str, text: &str, confidence: f32, from: &str, to: Option<&str>) -> Fact {
    Fact {
        id: Ulid::new(),
        subject_id: EntityId::from_name_and_type("Alice", "Person"),
        predicate: predicate.into(),
        object_id: EntityId::from_name_and_type("Paris", "Place"),
        fact_text: text.into(),
        confidence,
        valid_from_iso: from.into(),
        valid_to_iso: to.map(str::to_string),
    }
}

fn relation(predicate: &str, object: &str, confidence: f32) -> Relation {
    Relation {
        subject_id: EntityId::from_name_and_type("Alice", "Person"),
        predicate: predicate.into(),
        object_id: EntityId::from_name_and_type(object, "Place"),
        confidence,
        valid_from_iso: "2024-01-01T00:00:00Z".into(),
        valid_to_iso: None,
    }
}

fn batch(facts: Vec<Fact>, relations: Vec<Relation>) -> RawExtractionBatch {
    RawExtractionBatch {
        by_chunk: vec![RawExtraction {
            source_chunk_id: Ulid::new(),
            entities: vec![],
            relations,
            facts,
        }],
    }
}

/// Two facts with the identical (subject, predicate, object, interval) key —
/// different ids and phrasings — collapse to ONE survivor with the max
/// confidence. Nothing lands in needs_review (dupes are not invalid).
#[test]
fn duplicate_facts_collapse_keeping_max_confidence() {
    let v = validate(batch(
        vec![
            fact("lives_in", "Alice lives in Paris", 0.7, "2024-01-01T00:00:00Z", None),
            fact("lives_in", "Alice resides in Paris.", 0.9, "2024-01-01T00:00:00Z", None),
            fact("lives_in", "Alice lives in Paris (again)", 0.8, "2024-01-01T00:00:00Z", None),
        ],
        vec![],
    ));

    assert_eq!(v.facts.len(), 1, "identical triples must collapse; got {:?}", v.facts);
    assert!(
        (v.facts[0].confidence - 0.9).abs() < f32::EPSILON,
        "survivor must carry the MAX confidence; got {}",
        v.facts[0].confidence
    );
    assert!(v.needs_review.is_empty(), "duplicates are canonicalized, never demoted");
}

/// Same triple, DIFFERENT validity interval → distinct temporal assertions,
/// both preserved (bi-temporal correctness beats aggressive dedup).
#[test]
fn same_triple_different_interval_is_preserved() {
    let v = validate(batch(
        vec![
            fact(
                "lives_in",
                "Alice lives in Paris",
                0.9,
                "2020-01-01T00:00:00Z",
                Some("2022-01-01T00:00:00Z"),
            ),
            fact("lives_in", "Alice lives in Paris again", 0.9, "2024-01-01T00:00:00Z", None),
        ],
        vec![],
    ));
    assert_eq!(
        v.facts.len(),
        2,
        "distinct validity intervals must both survive; got {:?}",
        v.facts
    );
}

/// Duplicate relations collapse on the same full key; relations to DIFFERENT
/// objects are untouched here (overlapping-interval conflicts stay the
/// StructuralContradiction pass's job — all-or-nothing demotion, D-08/D-09).
#[test]
fn duplicate_relations_collapse_but_distinct_objects_flow_to_contradiction_pass() {
    let v = validate(batch(
        vec![],
        vec![relation("visited", "Paris", 0.6), relation("visited", "Paris", 0.8)],
    ));
    assert_eq!(v.relations.len(), 1, "identical relations must collapse; got {:?}", v.relations);
    assert!(
        (v.relations[0].confidence - 0.8).abs() < f32::EPSILON,
        "survivor must carry the MAX confidence"
    );
    assert!(v.needs_review.is_empty());

    // Different objects + overlapping interval = the PRE-EXISTING structural
    // contradiction demotion, not dedup. Pin that dedup does not swallow it.
    let v2 = validate(batch(
        vec![],
        vec![relation("visited", "Paris", 0.9), relation("visited", "Lyon", 0.9)],
    ));
    assert!(
        v2.relations.is_empty() && v2.needs_review.len() == 2,
        "conflicting objects must still demote via StructuralContradiction; got relations={:?} needs_review={}",
        v2.relations,
        v2.needs_review.len()
    );
}
