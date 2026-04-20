//! Episode-local Validator (D-08). Catches three classes of failure:
//!
//! 1. **Bi-temporal sanity** — `valid_from >= valid_to`
//! 2. **Structural contradictions within an Episode** — same `(subject_id,
//!    predicate)` with overlapping `[valid_from, valid_to]` and conflicting
//!    `object_id`s
//! 3. **GBNF post-validation** — parsed entity / relation / fact must satisfy
//!    the schema (non-empty name, non-empty entity_type / predicate, confidence
//!    in `[0, 1]`)
//!
//! Plus a fourth class that's the result of upstream backend failure rather
//! than schema invalidity:
//!
//! 4. **Transient-after-retry** — the cloud-api backend (D-21) emits a sentinel
//!    `Entity { entity_type: "__lunaris_sentinel__", name:
//!    "__transient_after_retry__", valid_from_iso: "transient: <error>" }` when
//!    the single retry exhausts; the validator detects this sentinel by
//!    `entity_type` and routes it to
//!    [`NeedsReviewReason::TransientAfterRetry`] carrying the wrapped error
//!    text. The sentinel NEVER lands in [`ValidatedExtraction::entities`].
//!
//! Cross-Episode contradictions (e.g., two Episodes claim different birth
//! years for Alice) are deferred to the Phase 4 Verifier per D-09 — out of
//! scope for v0. The validator IS Episode-local but works across all chunks
//! in a [`crate::RawExtractionBatch`].
//!
//! ## NeedsReview never silently dropped
//!
//! Per EXTRACT-05 + ROADMAP success criteria #2: invalid items are routed to
//! [`ValidatedExtraction::needs_review`] with a structured reason — there is
//! no "skip validator" call site for the extractor backends. Plan 03-03's
//! ingest fan-out reads ONLY `validated.entities/relations/facts` for
//! WriteOps; needs_review items emit a `__lunaris_verify__` MQ message via
//! `StoragePort::publish` so the Phase 4 Verifier worker can pick them up
//! later (D-19 Phase 4 hook).

#![allow(clippy::too_many_lines)]

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{Entity, EntityId, ExtractionBatch, Fact, RawExtractionBatch, Relation};

/// Sentinel reserved for the cloud-api retry-exhaust path (D-21 + W-1 fix).
/// The cloud-api backend emits an [`Entity`] with this `entity_type` and the
/// reserved `name` below to communicate "transient failure after 1 retry" up
/// to the validator without altering the trait return type. The validator
/// detects the pair and routes to [`NeedsReviewReason::TransientAfterRetry`].
pub const TRANSIENT_SENTINEL_TYPE: &str = "__lunaris_sentinel__";
/// See [`TRANSIENT_SENTINEL_TYPE`].
pub const TRANSIENT_SENTINEL_NAME: &str = "__transient_after_retry__";

/// Structured reason explaining why an extraction was routed to
/// [`ValidatedExtraction::needs_review`]. Each variant maps onto a
/// blueprint §5.2 / D-08 / D-21 NeedsReview class.
///
/// Implements [`std::error::Error`] via `thiserror` so callers can format the
/// reason directly into a `tracing::warn!` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum NeedsReviewReason {
    /// `valid_from >= valid_to` for an [`Entity`] / [`Relation`] / [`Fact`].
    #[error("invalid bi-temporal interval: valid_from {valid_from} >= valid_to {valid_to:?}")]
    InvalidBitemporal { valid_from: String, valid_to: Option<String> },

    /// Two or more [`Relation`]s on the same `(subject_id, predicate)` with
    /// overlapping `[valid_from, valid_to]` and conflicting `object_id`s.
    /// Per D-08 the validator demotes ALL conflicting entries (can't pick a
    /// winner without the Phase 4 Verifier).
    #[error(
        "structural contradiction: ({subject}, {predicate}) has overlapping objects {conflict:?}"
    )]
    StructuralContradiction { subject: EntityId, predicate: String, conflict: Vec<EntityId> },

    /// Parsed extraction violates the GBNF schema (empty name, empty
    /// predicate, empty fact_text, confidence outside `[0, 1]`).
    #[error("GBNF schema failure at {schema_path}: {error}")]
    GbnfFailure { schema_path: String, error: String },

    /// Cloud-API extractor exhausted its single retry budget per D-21.
    /// Carries the wrapped error text so the Phase 4 Verifier worker can
    /// triage which provider+model failed.
    #[error("extractor transient failure after 1 retry: {0}")]
    TransientAfterRetry(String),
}

/// One failed extraction with the reason it was demoted. Preserves the raw
/// item so the Phase 4 Verifier (Plan 04-XX) can re-extract / arbitrate.
#[derive(Clone, Debug, PartialEq)]
pub enum NeedsReviewItem {
    Entity { reason: NeedsReviewReason, raw: Entity },
    Relation { reason: NeedsReviewReason, raw: Relation },
    Fact { reason: NeedsReviewReason, raw: Fact },
}

/// What [`validate`] produces. The `entities`/`relations`/`facts` lists hold
/// items that passed validation and are safe to write to storage; the
/// `needs_review` list holds items that failed with a structured reason so the
/// ingest fan-out can publish them to the verify queue (D-19).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ValidatedExtraction {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
    pub facts: Vec<Fact>,
    pub needs_review: Vec<NeedsReviewItem>,
}

/// Validate an Episode-local extraction batch. Routes invalid items to
/// `needs_review` with a structured reason; never drops silently.
///
/// See module rustdoc for the four classes of failure handled.
pub fn validate(batch: RawExtractionBatch) -> ValidatedExtraction {
    let mut out = ValidatedExtraction::default();

    // (object_id, (valid_from, valid_to)) — one entry per relation in a
    // (subject_id, predicate) bucket. The interval bound is kept as Strings
    // so the lex-sort comparison matches the bi-temporal sanity path above.
    type RelationInterval = (EntityId, (String, Option<String>));

    // (subject_id, predicate) → Vec<RelationInterval>. Built during the
    // relation walk; used after the per-chunk loop to detect structural
    // contradictions WITHIN this Episode.
    let mut bucket: HashMap<(EntityId, String), Vec<RelationInterval>> = HashMap::new();

    for chunk in batch.by_chunk {
        // ---------- Entities ----------
        for e in chunk.entities {
            // (4) Sentinel detection — D-21 cloud-api retry-exhaust marker.
            // Route to TransientAfterRetry with the wrapped error text from
            // valid_from_iso. The sentinel is checked BEFORE the bi-temporal
            // / GBNF checks so it never lands in `out.entities`.
            if e.entity_type == TRANSIENT_SENTINEL_TYPE && e.name == TRANSIENT_SENTINEL_NAME {
                let err_text = e
                    .valid_from_iso
                    .strip_prefix("transient: ")
                    .map(str::to_string)
                    .unwrap_or_else(|| e.valid_from_iso.clone());
                out.needs_review.push(NeedsReviewItem::Entity {
                    reason: NeedsReviewReason::TransientAfterRetry(err_text),
                    raw: e,
                });
                continue;
            }

            // (1) Bi-temporal sanity. RFC3339 with the same offset (`Z`) lex-
            // sorts correctly; same-offset is the convention emitted by the
            // GBNF grammar (entities.gbnf valid_from_iso/valid_to_iso fields).
            if let Some(vt) = &e.valid_to_iso
                && &e.valid_from_iso >= vt
            {
                out.needs_review.push(NeedsReviewItem::Entity {
                    reason: NeedsReviewReason::InvalidBitemporal {
                        valid_from: e.valid_from_iso.clone(),
                        valid_to: e.valid_to_iso.clone(),
                    },
                    raw: e,
                });
                continue;
            }

            // (3) GBNF post-validation. Empty name / empty type / out-of-range
            // confidence are all schema violations — even if grammar-constrained
            // sampling was used, the post-hoc check catches the case where
            // the GBNF binding silently failed (D-05 fallback path).
            if e.name.is_empty() || e.entity_type.is_empty() || !(0.0..=1.0).contains(&e.confidence)
            {
                let err = format!(
                    "invalid name/type/confidence: name={:?} type={:?} conf={}",
                    e.name, e.entity_type, e.confidence
                );
                out.needs_review.push(NeedsReviewItem::Entity {
                    reason: NeedsReviewReason::GbnfFailure {
                        schema_path: "entities.gbnf".into(),
                        error: err,
                    },
                    raw: e,
                });
                continue;
            }

            out.entities.push(e);
        }

        // ---------- Relations ----------
        for r in chunk.relations {
            // (1) Bi-temporal sanity
            if let Some(vt) = &r.valid_to_iso
                && &r.valid_from_iso >= vt
            {
                out.needs_review.push(NeedsReviewItem::Relation {
                    reason: NeedsReviewReason::InvalidBitemporal {
                        valid_from: r.valid_from_iso.clone(),
                        valid_to: r.valid_to_iso.clone(),
                    },
                    raw: r,
                });
                continue;
            }

            // (3) GBNF post-validation
            if r.predicate.is_empty() || !(0.0..=1.0).contains(&r.confidence) {
                let err = format!(
                    "invalid predicate/confidence: pred={:?} conf={}",
                    r.predicate, r.confidence
                );
                out.needs_review.push(NeedsReviewItem::Relation {
                    reason: NeedsReviewReason::GbnfFailure {
                        schema_path: "relations.gbnf".into(),
                        error: err,
                    },
                    raw: r,
                });
                continue;
            }

            // Track for episode-local contradiction detection (D-08 #2).
            // We push BEFORE landing in `out.relations` so the bucket sees the
            // full set; the demotion pass below moves any conflicting entries
            // out of `out.relations` and into `needs_review`.
            bucket
                .entry((r.subject_id, r.predicate.clone()))
                .or_default()
                .push((r.object_id, (r.valid_from_iso.clone(), r.valid_to_iso.clone())));

            out.relations.push(r);
        }

        // ---------- Facts ----------
        for f in chunk.facts {
            // (1) Bi-temporal sanity
            if let Some(vt) = &f.valid_to_iso
                && &f.valid_from_iso >= vt
            {
                out.needs_review.push(NeedsReviewItem::Fact {
                    reason: NeedsReviewReason::InvalidBitemporal {
                        valid_from: f.valid_from_iso.clone(),
                        valid_to: f.valid_to_iso.clone(),
                    },
                    raw: f,
                });
                continue;
            }

            // (3) GBNF post-validation. Facts ALSO require non-empty fact_text
            // — an empty fact body is useless to retrieval.
            if f.predicate.is_empty()
                || f.fact_text.is_empty()
                || !(0.0..=1.0).contains(&f.confidence)
            {
                let err = format!(
                    "invalid predicate/fact_text/confidence: pred={:?} fact_text_len={} conf={}",
                    f.predicate,
                    f.fact_text.len(),
                    f.confidence
                );
                out.needs_review.push(NeedsReviewItem::Fact {
                    reason: NeedsReviewReason::GbnfFailure {
                        schema_path: "relations.gbnf".into(),
                        error: err,
                    },
                    raw: f,
                });
                continue;
            }

            out.facts.push(f);
        }
    }

    // ---------- (2) Episode-local structural contradiction detection ----------
    //
    // For each (subject_id, predicate) bucket, check whether any two entries
    // have OVERLAPPING [valid_from, valid_to] AND DIFFERENT object_ids. If so,
    // demote ALL entries in the bucket to `needs_review` with the
    // StructuralContradiction reason (per-bucket all-or-nothing because we
    // can't pick a winner without the Phase 4 Verifier; D-09).
    let mut to_demote: HashSet<(EntityId, String)> = HashSet::new();
    for ((subj, pred), entries) in &bucket {
        if entries.len() < 2 {
            continue;
        }
        // Pairwise overlap check — O(n^2) per bucket. Episode-local buckets
        // are tiny (typical chunk yields < 10 relations on the same
        // subject/predicate), so the quadratic cost is fine.
        let mut found_conflict = false;
        for i in 0..entries.len() {
            if found_conflict {
                break;
            }
            for j in (i + 1)..entries.len() {
                let (obj_i, (vf_i, vt_i)) = &entries[i];
                let (obj_j, (vf_j, vt_j)) = &entries[j];
                if obj_i == obj_j {
                    // Same object — restating, not contradicting.
                    continue;
                }
                // Overlap if max(vf_i, vf_j) < min(vt_i, vt_j); treat None as +infinity.
                let later_start = std::cmp::max(vf_i, vf_j);
                let earlier_end_opt: Option<&String> = match (vt_i, vt_j) {
                    (Some(a), Some(b)) => Some(std::cmp::min(a, b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                let overlaps = match earlier_end_opt {
                    Some(end) => later_start < end,
                    None => true,
                };
                if overlaps {
                    to_demote.insert((*subj, pred.clone()));
                    found_conflict = true;
                    break;
                }
            }
        }
    }

    if !to_demote.is_empty() {
        let kept = std::mem::take(&mut out.relations);
        for r in kept {
            let key = (r.subject_id, r.predicate.clone());
            if to_demote.contains(&key) {
                let conflict: Vec<EntityId> = bucket
                    .get(&key)
                    .map(|v| v.iter().map(|(o, _)| *o).collect())
                    .unwrap_or_default();
                out.needs_review.push(NeedsReviewItem::Relation {
                    reason: NeedsReviewReason::StructuralContradiction {
                        subject: r.subject_id,
                        predicate: r.predicate.clone(),
                        conflict,
                    },
                    raw: r,
                });
            } else {
                out.relations.push(r);
            }
        }
    }

    out
}

/// Project a [`ValidatedExtraction`] into a flat [`ExtractionBatch`] (the shape
/// the Plan 03-03 ingest fan-out converts into `WriteOp`s). `needs_review` is
/// dropped by this conversion — the caller is expected to publish those items
/// to `__lunaris_verify__` separately per D-19.
pub fn into_batch(v: &ValidatedExtraction) -> ExtractionBatch {
    ExtractionBatch {
        entities: v.entities.clone(),
        relations: v.relations.clone(),
        facts: v.facts.clone(),
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests live here; the full table-driven Validator suite is in
    //! `tests/validator.rs` (Task 3 of Plan 03-01).
    use super::*;
    use crate::types::RawExtraction;
    use ulid::Ulid;

    #[test]
    fn empty_batch_produces_empty_validated() {
        let v = validate(RawExtractionBatch::default());
        assert!(v.entities.is_empty());
        assert!(v.relations.is_empty());
        assert!(v.facts.is_empty());
        assert!(v.needs_review.is_empty());
    }

    #[test]
    fn into_batch_drops_needs_review() {
        let v = ValidatedExtraction {
            entities: vec![],
            relations: vec![],
            facts: vec![],
            needs_review: vec![NeedsReviewItem::Entity {
                reason: NeedsReviewReason::TransientAfterRetry("HTTP 503".into()),
                raw: Entity {
                    id: EntityId::from_name_and_type(
                        TRANSIENT_SENTINEL_NAME,
                        TRANSIENT_SENTINEL_TYPE,
                    ),
                    name: TRANSIENT_SENTINEL_NAME.into(),
                    aliases: vec![],
                    entity_type: TRANSIENT_SENTINEL_TYPE.into(),
                    confidence: 0.0,
                    valid_from_iso: "transient: HTTP 503".into(),
                    valid_to_iso: None,
                },
            }],
        };
        let b = into_batch(&v);
        assert!(b.entities.is_empty());
        assert!(b.relations.is_empty());
        assert!(b.facts.is_empty());
    }

    #[test]
    fn happy_path_single_entity_passes() {
        let entity = Entity {
            id: EntityId::from_name_and_type("Alice", "Person"),
            name: "Alice".into(),
            aliases: vec![],
            entity_type: "Person".into(),
            confidence: 0.9,
            valid_from_iso: "2024-01-01T00:00:00Z".into(),
            valid_to_iso: None,
        };
        let batch = RawExtractionBatch {
            by_chunk: vec![RawExtraction {
                source_chunk_id: Ulid::new(),
                entities: vec![entity.clone()],
                relations: vec![],
                facts: vec![],
            }],
        };
        let v = validate(batch);
        assert_eq!(v.entities, vec![entity]);
        assert!(v.needs_review.is_empty());
    }
}
