//! Verify parity gate — does `LlmVerifier(CandleBackend("gemma-3-27b-it"))`
//! produce the same arbitration decisions as the legacy [`CandleGemma3_27B`]
//! over a fixed [`NeedsReviewItem`] fixture?
//!
//! This is the **evidence** Phase 12 demands before the legacy
//! `CandleGemma3_27B` / `OllamaVerifier` / `CloudApiVerifier` are
//! re-implemented as thin wrappers around `Arc<dyn LlmBackend>` and their
//! duplicated load / forward / decode code is deleted. Without this gate,
//! the wrapper migration would be a verdict-quality regression masquerading
//! as a refactor — the legacy candle path embeds the arbitration prompt
//! directly and the wrapper must preserve that bit-for-bit.
//!
//! ## Skip policy
//!
//! SKIPs cleanly with `eprintln!` when `LUNARIS_VERIFY_GEMMA_27B_PATH` is
//! unset — mirrors `quality_gate_4b_vs_27b` and
//! `extractor_contract::candle_extracts_real_batch` discipline so CI
//! default jobs don't fail on missing model weights.
//!
//! ## Env vars
//!
//! ```text
//! LUNARIS_VERIFY_GEMMA_27B_PATH=/path/to/gemma-3-27b-it/   # required; SKIP if unset
//! LUNARIS_VERIFY_PARITY_MIN_AGREEMENT_RATIO=0.95            # optional, default 0.95
//! ```
//!
//! ## Pass criterion
//!
//! Per-item agreement must reach at least `MIN_AGREEMENT_RATIO` (default
//! 0.95). "Agreement" on a single item means EITHER:
//!
//! - Both decisions are deferred (`!applies()`), OR
//! - Both decisions `applies()` AND `winner_id` is equal on both.
//!
//! Any other combination (one arbitrates, the other defers; or both
//! arbitrate but pick different `winner_id`s) counts as disagreement.
//!
//! Confidence floats / `decided_at_iso` timestamps / `reason` strings are
//! NOT compared — those shift across model temperatures and are not
//! load-bearing for the wrapper-equivalence claim. The claim under test is
//! "arbitration shape is preserved across the wrapper migration."
//!
//! A mismatch fails the assertion with the message
//! **"Phase 12 duplication-delete BLOCKED"** so the blockage is
//! immediately visible in CI output.

#![cfg(all(feature = "candle", feature = "verifier-it"))]

use std::sync::Arc;

use lunaris_extract::{EntityId, Fact, NeedsReviewItem, NeedsReviewReason};
use lunaris_llm::{CandleBackend, CandleBackendOpts, LlmBackend};
use lunaris_verify::{
    CandleGemma3_27B, CandleGemma3_27BOpts, LlmVerifier, LlmVerifierOpts, Verifier,
};
use ulid::Ulid;

const DEFAULT_MIN_AGREEMENT_RATIO: f64 = 0.95;

#[tokio::test]
async fn llm_verifier_agrees_with_legacy_candle_27b_on_fixture() {
    let path = match std::env::var("LUNARIS_VERIFY_GEMMA_27B_PATH").ok() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP llm_verifier_agrees_with_legacy_candle_27b_on_fixture — \
                 set LUNARIS_VERIFY_GEMMA_27B_PATH to the gemma-3-27b-it weights dir"
            );
            return;
        }
    };
    let min_ratio: f64 = std::env::var("LUNARIS_VERIFY_PARITY_MIN_AGREEMENT_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MIN_AGREEMENT_RATIO);

    // ── Legacy path — CandleGemma3_27B (Phase 4 implementation) ─────────────
    let legacy = CandleGemma3_27B::new(CandleGemma3_27BOpts {
        model_path: Some(path.clone().into()),
        device: candle_core::Device::Cpu,
        ..CandleGemma3_27BOpts::default()
    })
    .await
    .expect("legacy CandleGemma3_27B load");

    // ── Wrapper path — LlmVerifier(CandleBackend("gemma-3-27b-it")) ──────────
    let backend: Arc<dyn LlmBackend> = Arc::new(
        CandleBackend::new(CandleBackendOpts {
            model_name: "gemma-3-27b-it".into(),
            model_path: path.into(),
            device: candle_core::Device::Cpu,
        })
        .await
        .expect("CandleBackend 27B load"),
    );
    // timeout_ms bumped to 4000 ms — 27B is ~7-10× slower than 4B.
    let wrapper = LlmVerifier::with_opts(
        backend,
        LlmVerifierOpts { timeout_ms: 4000, ..LlmVerifierOpts::default() },
    );

    let fixture = fixture_items();
    let total = fixture.len();
    let mut agree = 0usize;
    let mut log = Vec::with_capacity(total);

    for item in fixture {
        let d_legacy = legacy.verify(item.clone()).await.expect("legacy 27B verify");
        let d_wrapper = wrapper.verify(item.clone()).await.expect("wrapper verify");

        // Agreement: both deferred = match; both applies() AND winner_id
        // equal = match; everything else is a mismatch.
        let matched = match (d_legacy.applies(), d_wrapper.applies()) {
            (false, false) => true,
            (true, true) => d_legacy.winner_id == d_wrapper.winner_id,
            _ => false,
        };
        if matched {
            agree += 1;
        }
        log.push((matched, d_legacy.applies(), d_wrapper.applies()));
    }

    let ratio = agree as f64 / total as f64;
    println!(
        "verify parity: legacy CandleGemma3_27B vs LlmVerifier agreement = \
         {agree}/{total} = {:.2}% (threshold {:.2}%)",
        ratio * 100.0,
        min_ratio * 100.0,
    );
    for (i, (m, l_app, w_app)) in log.iter().enumerate() {
        println!("  item {i}: agree={m}  legacy_applies={l_app}  wrapper_applies={w_app}");
    }

    assert!(
        ratio >= min_ratio,
        "LlmVerifier diverges from legacy CandleGemma3_27B: agreement {:.2}% < threshold {:.2}%. \
         Phase 12 duplication-delete BLOCKED — investigate prompt / arbitration-JSON drift first.",
        ratio * 100.0,
        min_ratio * 100.0,
    );
}

/// Hand-crafted [`NeedsReviewItem`] fixture covering all four
/// `NeedsReviewReason` variants. 5 items — small enough to keep two
/// real-27B-model passes under a reasonable wall-clock budget; broad enough
/// to exercise every code path in the arbitration-prompt template.
fn fixture_items() -> Vec<NeedsReviewItem> {
    let alice = EntityId::from_name_and_type("Alice", "Person");
    let bob = EntityId::from_name_and_type("Bob", "Person");
    let charlie = EntityId::from_name_and_type("Charlie", "Person");
    let paris = EntityId::from_name_and_type("Paris", "City");

    vec![
        // 1. GbnfFailure — empty fact_text; verifier should reject or defer.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "facts/0".into(),
                error: "empty fact_text".into(),
            },
            raw: fact(alice, "knows", bob, "", 0.9, "2025-01-01"),
        },
        // 2. InvalidBitemporal — valid_from after valid_to.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::InvalidBitemporal {
                valid_from: "2025-06-01".into(),
                valid_to: Some("2024-01-01".into()),
            },
            raw: fact(alice, "lived_in", paris, "Alice lived in Paris", 0.8, "2025-06-01"),
        },
        // 3. StructuralContradiction — two candidate winners on same
        //    (subject, predicate). Verifier should pick one or defer.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::StructuralContradiction {
                subject: alice,
                predicate: "is_married_to".into(),
                conflict: vec![bob, charlie],
            },
            raw: fact(alice, "is_married_to", bob, "Alice married Bob", 0.7, "2025-01-01"),
        },
        // 4. TransientAfterRetry — cloud-api or retry-exhaust signal.
        //    Verifier is expected to defer or emit a low-confidence verdict.
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::TransientAfterRetry("provider 503".into()),
            raw: fact(alice, "knows", bob, "Alice knows Bob", 0.5, "2025-01-01"),
        },
        // 5. GbnfFailure / low confidence — second GbnfFailure variant to
        //    ensure the parity gate exercises the variant more than once.
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
