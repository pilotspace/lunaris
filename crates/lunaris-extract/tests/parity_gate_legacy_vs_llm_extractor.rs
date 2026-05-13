//! Extract parity gate — does `LlmExtractor(CandleBackend("gemma-3-4b-it"))`
//! produce the same extractions as the legacy [`CandleGemma3_4B`] over a
//! fixed fixture?
//!
//! This is the **evidence** Phase 12 demands before
//! `CandleGemma3_4B` / `OllamaExtractor` / `CloudApiExtractor` are
//! re-implemented as thin wrappers around `Arc<dyn LlmBackend>` and their
//! duplicated load / forward / decode code is deleted. Without this gate,
//! the wrapper migration would be a prompt-quality regression
//! masquerading as a refactor — the legacy candle path embeds
//! `ENTITIES_GBNF + RELATIONS_GBNF` in the prompt and the wrapper must
//! preserve that bit-for-bit (now wired via `LlmExtractorOpts.gbnf` —
//! see commit feat(lunaris-extract): plumb GBNF through LlmExtractorOpts).
//!
//! ## Skip policy
//!
//! SKIPs cleanly with `eprintln!` when `LUNARIS_EXTRACT_GEMMA_PATH` is
//! unset — mirrors `extractor_contract::candle_extracts_real_batch`
//! discipline so CI default jobs don't fail on missing weights.
//!
//! ## Env vars
//!
//! ```text
//! LUNARIS_EXTRACT_GEMMA_PATH=/path/to/gemma-3-4b-it/
//! LUNARIS_EXTRACT_MIN_AGREEMENT_RATIO=0.95   # optional, default 0.95
//! ```
//!
//! ## Pass criterion
//!
//! Per-chunk schema-level agreement must hit at least
//! `MIN_AGREEMENT_RATIO` (default 0.95). "Agreement" on a single chunk
//! means BOTH:
//!
//! - The set of `(entity.name, entity.entity_type)` pairs matches, AND
//! - The set of `(subject_id, predicate, object_id)` relation triples
//!   matches.
//!
//! Confidence floats / `valid_*_iso` strings are NOT compared — those
//! shift across model temperatures and are not load-bearing for the
//! wrapper-equivalence claim. The claim under test is "structural
//! extraction shape is preserved across the wrapper migration."
//!
//! The threshold is lower than 1.0 because Gemma-3 4B is not perfectly
//! deterministic even at temperature 0.0 on CPU due to numerical
//! non-associativity. 0.95 is the production gate; 0.80 is acceptable
//! for a smoke run on a smaller fixture (override via env).

#![cfg(all(feature = "candle", feature = "extractor-it"))]

use std::collections::HashSet;
use std::sync::Arc;

use lunaris_extract::candle_gemma3::{ENTITIES_GBNF, RELATIONS_GBNF};
use lunaris_extract::{
    CandleGemma3_4B, CandleGemma3_4BOpts, ChunkInput, Extractor, LlmExtractor, LlmExtractorOpts,
};
use lunaris_llm::{CandleBackend, CandleBackendOpts, LlmBackend};
use ulid::Ulid;

const DEFAULT_MIN_AGREEMENT_RATIO: f64 = 0.95;

/// Build the GBNF grammar text the Phase 12 wrapper migration will
/// inject via `LlmExtractorOpts.gbnf`. The field is `&'static str`, so
/// we `Box::leak` the once-built concatenation. Acceptable in a
/// run-once test; the future wrapper substitutes a real `const` once
/// the legacy grammars are reorganized.
fn wrapper_gbnf() -> &'static str {
    Box::leak(format!("{ENTITIES_GBNF}\n{RELATIONS_GBNF}").into_boxed_str())
}

#[tokio::test]
async fn llm_extractor_agrees_with_legacy_candle_on_fixture() {
    let path = match std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").ok() {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP llm_extractor_agrees_with_legacy_candle_on_fixture — \
                 set LUNARIS_EXTRACT_GEMMA_PATH to the gemma-3-4b-it weights dir"
            );
            return;
        }
    };
    let min_ratio: f64 = std::env::var("LUNARIS_EXTRACT_MIN_AGREEMENT_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MIN_AGREEMENT_RATIO);

    let legacy = CandleGemma3_4B::new(CandleGemma3_4BOpts {
        model_path: Some(path.clone().into()),
        device: candle_core::Device::Cpu,
        ..CandleGemma3_4BOpts::default()
    })
    .await
    .expect("legacy 4B load");

    let backend: Arc<dyn LlmBackend> = Arc::new(
        CandleBackend::new(CandleBackendOpts {
            model_name: "gemma-3-4b-it".into(),
            model_path: path.into(),
            device: candle_core::Device::Cpu,
        })
        .await
        .expect("CandleBackend 4B load"),
    );
    // GBNF passthrough is the load-bearing setting — without this the
    // wrapper's prompt diverges from the legacy embed and the
    // assertion ratio will (correctly) drop.
    let wrapper = LlmExtractor::with_opts(
        backend,
        LlmExtractorOpts { gbnf: Some(wrapper_gbnf()), ..LlmExtractorOpts::default() },
    );

    let fixture = fixture_chunks();
    let episode_id = Ulid::new();
    let legacy_batch = legacy.extract(episode_id, &fixture).await.expect("legacy extract");
    let wrapper_batch = wrapper.extract(episode_id, &fixture).await.expect("wrapper extract");

    assert_eq!(legacy_batch.by_chunk.len(), wrapper_batch.by_chunk.len());

    let total = legacy_batch.by_chunk.len();
    let mut agree = 0usize;
    for (i, (l, w)) in
        legacy_batch.by_chunk.iter().zip(wrapper_batch.by_chunk.iter()).enumerate()
    {
        let l_ents: HashSet<(String, String)> =
            l.entities.iter().map(|e| (e.name.clone(), e.entity_type.clone())).collect();
        let w_ents: HashSet<(String, String)> =
            w.entities.iter().map(|e| (e.name.clone(), e.entity_type.clone())).collect();
        let l_rels: HashSet<(_, String, _)> =
            l.relations.iter().map(|r| (r.subject_id, r.predicate.clone(), r.object_id)).collect();
        let w_rels: HashSet<(_, String, _)> =
            w.relations.iter().map(|r| (r.subject_id, r.predicate.clone(), r.object_id)).collect();

        let matched = l_ents == w_ents && l_rels == w_rels;
        if matched {
            agree += 1;
        }
        println!(
            "  chunk {i}: agree={matched}  legacy_ents={}  wrap_ents={}  legacy_rels={}  wrap_rels={}",
            l_ents.len(),
            w_ents.len(),
            l_rels.len(),
            w_rels.len(),
        );
    }

    let ratio = agree as f64 / total as f64;
    println!(
        "extract parity: legacy vs LlmExtractor agreement = {agree}/{total} = {:.2}% (threshold {:.2}%)",
        ratio * 100.0,
        min_ratio * 100.0,
    );
    assert!(
        ratio >= min_ratio,
        "LlmExtractor diverges from legacy CandleGemma3_4B: agreement {:.2}% < threshold {:.2}%. \
         Phase 12 duplication-delete is BLOCKED — investigate prompt / GBNF drift first.",
        ratio * 100.0,
        min_ratio * 100.0,
    );
}

/// Hand-crafted fixture. Small enough to keep two real-model passes
/// under a minute on CPU; broad enough to exercise different entity
/// types, multi-relation chunks, and a near-empty chunk that should
/// produce zero extractions on both paths.
fn fixture_chunks() -> Vec<ChunkInput> {
    vec![
        ChunkInput {
            chunk_id: Ulid::new(),
            heading_path: vec!["meetings".into()],
            text: "Alice met Bob in Paris on 2025-01-15 to discuss the merger.".into(),
        },
        ChunkInput {
            chunk_id: Ulid::new(),
            heading_path: vec!["employment".into()],
            text: "Charlie joined Acme Corp as CTO in March 2025.".into(),
        },
        ChunkInput {
            chunk_id: Ulid::new(),
            heading_path: vec!["empty".into()],
            text: ".".into(),
        },
    ]
}
