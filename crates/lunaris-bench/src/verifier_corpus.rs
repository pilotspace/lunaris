//! RFC 0006 §4 — deterministic 100-item NeedsReviewItem corpus generator.
//!
//! Drives the verifier divergence comparison (`lunaris_verify::divergence::
//! compare_verifiers`). The corpus mixes all four [`NeedsReviewReason`]
//! variants so the comparison hits the full surface of cases the production
//! verifier worker sees.
//!
//! ## Composition (deterministic mix)
//!
//! | Count | Reason variant            | What it stresses                                          |
//! |-------|---------------------------|-----------------------------------------------------------|
//! |   25  | `GbnfFailure`             | Schema-shaped extraction errors (empty name, bad confidence) |
//! |   25  | `InvalidBitemporal`       | Bi-temporal interval violations (`valid_from >= valid_to`)    |
//! |   25  | `StructuralContradiction` | Overlapping `(subject, predicate)` with conflicting objects   |
//! |   25  | `TransientAfterRetry`     | Cloud-API exhaust-after-1-retry path                           |
//!
//! ## Determinism
//!
//! Seeded `ChaCha20Rng` — same `seed: u64` produces byte-identical corpus.
//! The default seed is `0` so test invocations match the benchmark
//! invocations. This is the same discipline `corpus.rs` uses for the
//! 1M-fact generator (`build_one_million_fact_corpus(seed=0)`).

use lunaris_extract::types::{Entity, EntityId, Fact, Relation};
use lunaris_extract::{NeedsReviewItem, NeedsReviewReason};
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

/// Deterministic seed used by [`verifier_corpus_100`]. Mirrors
/// `crate::corpus::SEED` so a single audit-trail logs both 1M-fact and
/// verifier-corpus runs as "seed=0."
pub const SEED: u64 = 0;

/// Default corpus size — RFC 0006 §4 specifies 100 items. Exposed as a
/// const so callers don't have to memorise the magic number.
pub const RFC_0006_CORPUS_SIZE: usize = 100;

/// Build the canonical 100-item RFC 0006 §4 corpus. Composition:
/// 25 `GbnfFailure`, 25 `InvalidBitemporal`, 25 `StructuralContradiction`,
/// 25 `TransientAfterRetry`. Order is deterministic — variants are
/// interleaved 25:25:25:25 rather than block-grouped so the comparison
/// harness sees a representative mix mid-run.
#[must_use]
pub fn verifier_corpus_100() -> Vec<NeedsReviewItem> {
    verifier_corpus(RFC_0006_CORPUS_SIZE, SEED)
}

/// Build a corpus of `n` items with explicit seed. Useful for stress
/// runs (n=1000) or seed-sensitivity sweeps. `n` does NOT have to be
/// divisible by 4 — the variant cycle handles any remainder.
#[must_use]
pub fn verifier_corpus(n: usize, seed: u64) -> Vec<NeedsReviewItem> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let variant = i % 4;
        out.push(match variant {
            0 => mk_gbnf_failure(&mut rng, i),
            1 => mk_invalid_bitemporal(&mut rng, i),
            2 => mk_structural_contradiction(&mut rng, i),
            _ => mk_transient_after_retry(&mut rng, i),
        });
    }
    out
}

// ───────────────────────── per-variant generators ─────────────────────────

fn mk_gbnf_failure(rng: &mut ChaCha20Rng, idx: usize) -> NeedsReviewItem {
    NeedsReviewItem::Entity {
        reason: NeedsReviewReason::GbnfFailure {
            schema_path: "/entity/confidence".to_string(),
            error: format!("confidence {} out of [0,1]", rand_bad_confidence(rng)),
        },
        raw: synthetic_entity(rng, idx, "Person"),
    }
}

fn mk_invalid_bitemporal(rng: &mut ChaCha20Rng, idx: usize) -> NeedsReviewItem {
    // `valid_from >= valid_to` — pick from later than to.
    let from = iso_date(2026, 6, 1);
    let to = Some(iso_date(2026, 1, 1));
    NeedsReviewItem::Fact {
        reason: NeedsReviewReason::InvalidBitemporal {
            valid_from: from.clone(),
            valid_to: to.clone(),
        },
        raw: Fact {
            id: ulid::Ulid::new(),
            subject_id: EntityId::from_name_and_type(&format!("Subject_{idx}"), "Person"),
            predicate: pick_predicate(rng),
            object_id: EntityId::from_name_and_type(&format!("Object_{idx}"), "Place"),
            fact_text: format!("synthetic fact {idx} with invalid temporal interval"),
            confidence: 0.8 + (rng.random::<f32>() * 0.2_f32).min(0.19),
            valid_from_iso: from,
            valid_to_iso: to,
        },
    }
}

fn mk_structural_contradiction(rng: &mut ChaCha20Rng, idx: usize) -> NeedsReviewItem {
    let subject = EntityId::from_name_and_type(&format!("Subject_{idx}"), "Person");
    let conflict_a = EntityId::from_name_and_type(&format!("ObjectA_{idx}"), "Organization");
    let conflict_b = EntityId::from_name_and_type(&format!("ObjectB_{idx}"), "Organization");
    let predicate = pick_predicate(rng);
    NeedsReviewItem::Relation {
        reason: NeedsReviewReason::StructuralContradiction {
            subject,
            predicate: predicate.clone(),
            conflict: vec![conflict_a, conflict_b],
        },
        raw: Relation {
            subject_id: subject,
            predicate,
            object_id: conflict_a,
            confidence: 0.7 + (rng.random::<f32>() * 0.3_f32).min(0.29),
            valid_from_iso: iso_date(2024, 1, 1),
            valid_to_iso: None,
        },
    }
}

fn mk_transient_after_retry(rng: &mut ChaCha20Rng, idx: usize) -> NeedsReviewItem {
    let provider = match idx % 3 {
        0 => "anthropic",
        1 => "openai",
        _ => "gemini",
    };
    let etype = pick_entity_type(rng);
    NeedsReviewItem::Entity {
        reason: NeedsReviewReason::TransientAfterRetry(format!(
            "{provider}: HTTP 503 (retry budget exhausted)"
        )),
        raw: synthetic_entity(rng, idx, etype),
    }
}

// ───────────────────────── helpers ─────────────────────────

fn synthetic_entity(rng: &mut ChaCha20Rng, idx: usize, entity_type: &str) -> Entity {
    let name = format!("{}_Entity_{idx}", pick_first_name(rng));
    Entity {
        id: EntityId::from_name_and_type(&name, entity_type),
        name,
        aliases: vec![],
        entity_type: entity_type.to_string(),
        confidence: 0.5 + rng.random::<f32>() * 0.5_f32,
        valid_from_iso: iso_date(2024, rng.random_range(1..=12), rng.random_range(1..=28)),
        valid_to_iso: None,
    }
}

fn iso_date(y: u16, m: u32, d: u32) -> String {
    format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
}

fn rand_bad_confidence(rng: &mut ChaCha20Rng) -> f32 {
    // Negative or > 1 — both violate the GBNF schema.
    let sign: f32 = if rng.random::<bool>() { -1.0 } else { 2.0 };
    sign * (0.5 + rng.random::<f32>())
}

fn pick_first_name(rng: &mut ChaCha20Rng) -> &'static str {
    const NAMES: &[&str] =
        &["Alice", "Bob", "Carol", "Dave", "Erin", "Frank", "Grace", "Heidi", "Ivan", "Judy"];
    NAMES[rng.random_range(0..NAMES.len())]
}

fn pick_entity_type(rng: &mut ChaCha20Rng) -> &'static str {
    const TYPES: &[&str] = &["Person", "Organization", "Place", "Date", "Event"];
    TYPES[rng.random_range(0..TYPES.len())]
}

fn pick_predicate(rng: &mut ChaCha20Rng) -> String {
    const PREDS: &[&str] =
        &["works_at", "located_in", "born_on", "founded", "owns", "married_to", "reports_to"];
    PREDS[rng.random_range(0..PREDS.len())].to_string()
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_0006_corpus_is_exactly_100_items() {
        let corpus = verifier_corpus_100();
        assert_eq!(corpus.len(), RFC_0006_CORPUS_SIZE);
    }

    #[test]
    fn variant_mix_is_25_25_25_25() {
        let corpus = verifier_corpus_100();
        let (mut gbnf, mut bitemporal, mut structural, mut transient) = (0, 0, 0, 0);
        for item in &corpus {
            let reason = match item {
                NeedsReviewItem::Entity { reason, .. }
                | NeedsReviewItem::Relation { reason, .. }
                | NeedsReviewItem::Fact { reason, .. } => reason,
            };
            match reason {
                NeedsReviewReason::GbnfFailure { .. } => gbnf += 1,
                NeedsReviewReason::InvalidBitemporal { .. } => bitemporal += 1,
                NeedsReviewReason::StructuralContradiction { .. } => structural += 1,
                NeedsReviewReason::TransientAfterRetry(_) => transient += 1,
            }
        }
        assert_eq!(gbnf, 25, "GbnfFailure count");
        assert_eq!(bitemporal, 25, "InvalidBitemporal count");
        assert_eq!(structural, 25, "StructuralContradiction count");
        assert_eq!(transient, 25, "TransientAfterRetry count");
    }

    #[test]
    fn corpus_is_deterministic_for_seed_zero() {
        let a = verifier_corpus_100();
        let b = verifier_corpus_100();
        // Same seed → byte-identical entity names.
        let names_a: Vec<_> = a
            .iter()
            .filter_map(|i| match i {
                NeedsReviewItem::Entity { raw, .. } => Some(raw.name.clone()),
                _ => None,
            })
            .collect();
        let names_b: Vec<_> = b
            .iter()
            .filter_map(|i| match i {
                NeedsReviewItem::Entity { raw, .. } => Some(raw.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names_a, names_b, "same seed must produce byte-identical corpus");
    }

    #[test]
    fn corpus_seed_changes_output() {
        let a = verifier_corpus(20, 0);
        let b = verifier_corpus(20, 42);
        // Different seeds → at least one differing entity name. (Names
        // are random-picked; collision-free in 10 items by birthday.)
        let names_a: Vec<_> = a
            .iter()
            .filter_map(|i| match i {
                NeedsReviewItem::Entity { raw, .. } => Some(raw.name.clone()),
                _ => None,
            })
            .collect();
        let names_b: Vec<_> = b
            .iter()
            .filter_map(|i| match i {
                NeedsReviewItem::Entity { raw, .. } => Some(raw.name.clone()),
                _ => None,
            })
            .collect();
        assert_ne!(names_a, names_b, "different seeds must produce different corpora");
    }
}
