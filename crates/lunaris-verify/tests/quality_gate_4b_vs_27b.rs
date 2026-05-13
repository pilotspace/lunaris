//! ER-F1 quality gate — does Gemma-3 4B verify produce the same
//! arbitration verdicts as Gemma-3 27B on a fixed fixture?
//!
//! This test is the **evidence** the agreed migration plan demands before
//! `Pipeline::Verify.default_provider_and_model()` flips from 27B → 4B.
//! Without this gate, a silent default flip would degrade verify quality
//! by an unknown amount on real production traffic.
//!
//! ## Skip policy
//!
//! SKIPs cleanly with `eprintln!` when either of the two env vars is
//! unset — mirrors `lunaris-extract::extractor_contract` test discipline
//! so CI default jobs don't fail on missing weights.
//!
//! ## Env vars
//!
//! ```text
//! LUNARIS_VERIFY_GEMMA_4B_PATH=/path/to/gemma-3-4b-it/
//! LUNARIS_VERIFY_GEMMA_27B_PATH=/path/to/gemma-3-27b-it/
//! ```
//!
//! Each directory MUST contain `tokenizer.json`, `config.json`,
//! `model.safetensors`. Cache-miss errors include the actionable
//! `huggingface-cli download ...` hint.
//!
//! ## Pass criterion
//!
//! The 4B verifier must agree with the 27B verifier on at least
//! `MIN_AGREEMENT_RATIO` (default 0.80) of the fixture items. "Agree"
//! means BOTH:
//!
//! - Both decisions are `applies()` (= arbitrated), AND
//! - Both name the SAME winner ulid, OR
//! - Both decisions are deferred.
//!
//! A 4B "arbitrates winner=X" vs 27B "arbitrates winner=Y" counts as
//! disagreement. A 4B "deferred" vs 27B "arbitrates" counts as
//! disagreement (4B is being too cautious).
//!
//! Threshold tunable via `LUNARIS_VERIFY_MIN_AGREEMENT_RATIO` env (so
//! a tighter local test or a looser CI smoke can use the same fixture).

#![cfg(all(feature = "candle", feature = "verifier-it"))]

use std::sync::Arc;

use lunaris_extract::{EntityId, Fact, NeedsReviewItem, NeedsReviewReason};
use lunaris_llm::{CandleBackend, CandleBackendOpts, LlmBackend};
use lunaris_verify::{LlmVerifier, LlmVerifierOpts, Verifier};
use ulid::Ulid;

/// Default agreement threshold (proportion in `[0.0, 1.0]`). 0.80 is
/// the production gate; CI smoke tests can override via env.
const DEFAULT_MIN_AGREEMENT_RATIO: f64 = 0.80;

#[tokio::test]
async fn verify_4b_agrees_with_27b_on_fixture() {
    let path_4b = match std::env::var("LUNARIS_VERIFY_GEMMA_4B_PATH").ok() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP verify_4b_agrees_with_27b_on_fixture — set LUNARIS_VERIFY_GEMMA_4B_PATH"
            );
            return;
        }
    };
    let path_27b = match std::env::var("LUNARIS_VERIFY_GEMMA_27B_PATH").ok() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP verify_4b_agrees_with_27b_on_fixture — set LUNARIS_VERIFY_GEMMA_27B_PATH"
            );
            return;
        }
    };
    let min_ratio: f64 = std::env::var("LUNARIS_VERIFY_MIN_AGREEMENT_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MIN_AGREEMENT_RATIO);

    let backend_4b: Arc<dyn LlmBackend> = Arc::new(
        CandleBackend::new(CandleBackendOpts {
            model_name: "gemma-3-4b-it".into(),
            model_path: path_4b.into(),
            device: candle_core::Device::Cpu,
        })
        .await
        .expect("4B load"),
    );
    let backend_27b: Arc<dyn LlmBackend> = Arc::new(
        CandleBackend::new(CandleBackendOpts {
            model_name: "gemma-3-27b-it".into(),
            model_path: path_27b.into(),
            device: candle_core::Device::Cpu,
        })
        .await
        .expect("27B load"),
    );

    let verifier_4b = LlmVerifier::with_opts(
        backend_4b,
        LlmVerifierOpts { timeout_ms: 600, ..LlmVerifierOpts::default() },
    );
    let verifier_27b = LlmVerifier::with_opts(
        backend_27b,
        LlmVerifierOpts { timeout_ms: 4000, ..LlmVerifierOpts::default() },
    );

    let fixture = fixture_items();
    let total = fixture.len();
    let mut agree = 0usize;
    let mut log = Vec::with_capacity(total);

    for item in fixture {
        let d4 = verifier_4b.verify(item.clone()).await.expect("4B verify");
        let d27 = verifier_27b.verify(item.clone()).await.expect("27B verify");
        let matched = match (d4.applies(), d27.applies()) {
            (false, false) => true,
            (true, true) => d4.winner_id == d27.winner_id,
            _ => false,
        };
        if matched {
            agree += 1;
        }
        log.push((matched, d4.applies(), d27.applies()));
    }

    let ratio = agree as f64 / total as f64;
    println!(
        "ER-F1 gate: 4B vs 27B agreement = {agree}/{total} = {:.2}% (threshold {:.2}%)",
        ratio * 100.0,
        min_ratio * 100.0
    );
    for (i, (m, a4, a27)) in log.iter().enumerate() {
        println!("  item {i}: agree={m}  4B_applies={a4}  27B_applies={a27}");
    }
    assert!(
        ratio >= min_ratio,
        "4B verify regresses vs 27B: agreement {:.2}% < threshold {:.2}%. Default flip BLOCKED.",
        ratio * 100.0,
        min_ratio * 100.0
    );
}

/// Hand-crafted [`NeedsReviewItem`] fixture covering the four
/// `NeedsReviewReason` variants. Small (5 items) — meant to be
/// expanded as we collect production traffic samples. The threshold
/// gates on *ratio*, so 4-of-5 = 80% passes; 3-of-5 = 60% fails.
fn fixture_items() -> Vec<NeedsReviewItem> {
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let bob = EntityId::from_name_and_type("Bob", "Person");
    let charlie = EntityId::from_name_and_type("Charlie", "Person");
    let paris = EntityId::from_name_and_type("Paris", "City");

    vec![
        // 1. GBNF failure on a Fact — verifier should reject or defer.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "facts/0".into(),
                error: "empty fact_text".into(),
            },
            raw: fact(alice, "knows", bob, "", 0.9, "2025-01-01"),
        },
        // 2. Invalid bi-temporal — valid_from after valid_to.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::InvalidBitemporal {
                valid_from: "2025-06-01".into(),
                valid_to: Some("2024-01-01".into()),
            },
            raw: fact(alice, "lived_in", paris, "Alice lived in Paris", 0.8, "2025-06-01"),
        },
        // 3. Structural contradiction on (subject, predicate).
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::StructuralContradiction {
                subject: alice,
                predicate: "is_married_to".into(),
                conflict: vec![bob, charlie],
            },
            raw: fact(alice, "is_married_to", bob, "Alice married Bob", 0.7, "2025-01-01"),
        },
        // 4. Transient retry exhausted — likely deferred verdict.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::TransientAfterRetry("provider 503".into()),
            raw: fact(alice, "knows", bob, "Alice knows Bob", 0.5, "2025-01-01"),
        },
        // 5. Plain non-contradictory fact — verifier ideally picks the higher
        //    confidence side; deferral is acceptable too.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "facts/1".into(),
                error: "low confidence".into(),
            },
            raw: fact(bob, "born_in", paris, "Bob was born in Paris", 0.95, "1990-01-01"),
        },
    ]
}

fn fact(
    subject: EntityId,
    predicate: &str,
    object: EntityId,
    text: &str,
    confidence: f32,
    valid_from: &str,
) -> Fact {
    Fact {
        id: Ulid::new(),
        subject_id: subject,
        predicate: predicate.into(),
        object_id: object,
        fact_text: text.into(),
        confidence,
        valid_from_iso: valid_from.into(),
        valid_to_iso: None,
    }
}
