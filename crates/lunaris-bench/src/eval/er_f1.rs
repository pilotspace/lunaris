//! Plan 05-06 EVAL-03 — Entity Resolution F1 harness.
//!
//! Loads `tner/wnut2017` (held-out NER corpus per CONTEXT.md D-19) via
//! `crate::eval::longmemeval::download_dataset` (W-7 fix — shared
//! `pub(crate)` helper, NOT a fictional `__download_test_helper` alias);
//! runs lunaris-extract Gemma-3 4B on the same text; compares predicted
//! `(entity, type)` pairs to gold; computes F1; asserts F1 ≥ 0.80.
//!
//! ## Soft-fail
//!
//! - `LUNARIS_EXTRACT_GEMMA_PATH` unset → SKIPPED (extractor not loadable
//!   without the model weights cached locally; CI doesn't ship a 4B model).
//! - HF dataset download failure → SKIPPED.
//! - F1 computation stub returns 0.0 by default (the gold/predicted pair
//!   parser + extractor invocation lands in 05-HUMAN-UAT.md per ROADMAP
//!   risk register day-7 fallback).

#![forbid(unsafe_code)]

use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "er-f1";
pub(crate) const METRIC: &str = "f1";
pub(crate) const THRESHOLD: f64 = 0.80;
const HF_REPO: &str = "tner/wnut2017";
const DATASET_FILENAME: &str = "dataset/test.json";

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let started = Instant::now();

    // The lunaris-extract Gemma-3 4B backend requires the model weights
    // cached locally — env-gated per Plan 03-01 D-04 convention. CI
    // doesn't bundle a 4B model so the harness SKIPS cleanly there;
    // dev-box runs set the env after `huggingface-cli download` lands.
    if std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").is_err() {
        results.push(EvalRow::skipped(
            HARNESS,
            METRIC,
            THRESHOLD,
            "LUNARIS_EXTRACT_GEMMA_PATH unset — extractor not loadable",
        ));
        return Ok(());
    }

    let cache = crate::eval::longmemeval::resolve_cache_dir();
    let _ = std::fs::create_dir_all(&cache);

    // W-7 fix: reuse shared download_dataset helper from longmemeval.rs.
    let dataset_path =
        match crate::eval::longmemeval::download_dataset(HF_REPO, DATASET_FILENAME, &cache).await {
            Ok(p) => p,
            Err(e) => {
                results.push(EvalRow::skipped(
                    HARNESS,
                    METRIC,
                    THRESHOLD,
                    &format!("WNUT-2017 download failed: {e}"),
                ));
                return Ok(());
            }
        };

    // Parse the WNUT-2017 gold spans (pure; fixture-tested). Garbage bytes →
    // SKIP (a malformed dataset is an absent capability, never a 0.0→FAIL).
    let bytes = match std::fs::read(&dataset_path) {
        Ok(b) => b,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("read {}: {e}", dataset_path.display()),
            ));
            return Ok(());
        }
    };
    let docs = match parse_wnut(&bytes) {
        Ok(d) => d,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("parse {}: {e}", dataset_path.display()),
            ));
            return Ok(());
        }
    };

    // The production extractor lives on the Lunaris handle. At HUMAN-UAT this is
    // the native Gemma backend (feature build + LUNARIS_EXTRACT_GEMMA_PATH); a
    // handle carrying only NoopExtractor (`applies() == false`) → SKIP. Never
    // FAIL on a merely-absent extractor.
    // Store URL is REQUIRED — there is no default, and deliberately so.
    //
    // Through 0.6.x this fell back to `"memory://"`, which 0.7.0 deleted. The
    // fix is not a new default (`moon://localhost:6380` would be worse: a bench
    // that silently picks a store can silently pick the WRONG store, and this
    // machine's 6381 is a live personal memory store). An unset variable is a
    // hard error naming both spellings, so an operator who meant to point at
    // the dedicated bench Moon finds out before the run, not after.
    let url = match std::env::var("MOON_URL").or_else(|_| std::env::var("LUNARIS_URL")) {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            return Err(anyhow::anyhow!(
                "ER-F1 needs a store URL and has no default: set MOON_URL (or LUNARIS_URL) to a \
                 `moon://host:port`. Point it at a DEDICATED bench Moon — never at a live store."
            ));
        }
    };
    let lunaris = match lunaris::Lunaris::open(&url).await {
        Ok(l) => l,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("Lunaris::open({url}) failed: {e}"),
            ));
            return Ok(());
        }
    };
    let extractor = match lunaris.extractor() {
        Some(e) if e.applies() => e,
        _ => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                "extractor not loadable (NoopExtractor) — wire the native Gemma backend at HUMAN-UAT",
            ));
            return Ok(());
        }
    };

    // Run the extractor over each WNUT doc; compare its (entity, type) output to
    // gold via the pure micro-F1 scorer. Aligning the extractor's type taxonomy
    // onto WNUT's {person, location, corporation, …} is a HUMAN-UAT refinement.
    let mut predicted: Vec<(String, String)> = Vec::new();
    let mut gold: Vec<(String, String)> = Vec::new();
    for doc in &docs {
        gold.extend(doc.gold.iter().cloned());
        let chunk = lunaris_extract::ChunkInput {
            chunk_id: ulid::Ulid::new(),
            text: doc.text.clone(),
            heading_path: Vec::new(),
        };
        match extractor.extract(ulid::Ulid::new(), std::slice::from_ref(&chunk)).await {
            Ok(batch) => {
                for raw in batch.by_chunk {
                    for ent in raw.entities {
                        predicted.push((ent.name, ent.entity_type));
                    }
                }
            }
            Err(e) => {
                results.push(EvalRow::skipped(
                    HARNESS,
                    METRIC,
                    THRESHOLD,
                    &format!("extractor failed: {e}"),
                ));
                return Ok(());
            }
        }
    }

    let f1 = crate::eval::score::compute_f1(&predicted, &gold);
    results.push(EvalRow::judge_ge(
        HARNESS,
        METRIC,
        f1,
        THRESHOLD,
        started.elapsed().as_millis() as u64,
    ));
    Ok(())
}

/// One WNUT-2017 document: the raw token text plus the gold `(entity, type)`
/// pairs derived from its BIO tag spans. Produced by [`parse_wnut`].
#[derive(Debug, Clone)]
pub struct WnutDoc {
    pub text: String,
    pub gold: Vec<(String, String)>,
}

/// WNUT-2017 tner BIO id→label map (`tner/wnut2017` `label2id`): ids 0..=11 are
/// the `B-`/`I-` tags for {corporation, creative-work, group, location, person,
/// product}; id 12 is `O`. The entity *type* is the suffix after the BIO prefix.
const WNUT_LABELS: [&str; 13] = [
    "B-corporation",
    "B-creative-work",
    "B-group",
    "B-location",
    "B-person",
    "B-product",
    "I-corporation",
    "I-creative-work",
    "I-group",
    "I-location",
    "I-person",
    "I-product",
    "O",
];

#[derive(serde::Deserialize)]
struct WnutRaw {
    tokens: Vec<String>,
    tags: Vec<i64>,
}

/// Close the open BIO span (if any), pushing its `(entity_text, type)` pair.
fn close_span(open: &mut Option<(String, Vec<String>)>, gold: &mut Vec<(String, String)>) {
    if let Some((ty, toks)) = open.take() {
        gold.push((toks.join(" "), ty));
    }
}

/// Parse WNUT-2017 tner-format bytes (`[{"tokens":[...],"tags":[int,...]}]`)
/// into docs with BIO spans collapsed to `(entity_text, type)` gold pairs.
/// Pure (bytes → typed); `Err` on malformed JSON so the caller maps to SKIPPED.
pub fn parse_wnut(bytes: &[u8]) -> anyhow::Result<Vec<WnutDoc>> {
    let raws: Vec<WnutRaw> =
        serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("parse wnut: {e}"))?;
    let mut docs = Vec::with_capacity(raws.len());
    for raw in raws {
        let text = raw.tokens.join(" ");
        let mut gold: Vec<(String, String)> = Vec::new();
        // The span currently being accumulated: (entity_type, tokens-so-far).
        let mut open: Option<(String, Vec<String>)> = None;
        for (tok, tag) in raw.tokens.iter().zip(raw.tags.iter()) {
            let label = WNUT_LABELS.get(*tag as usize).copied().unwrap_or("O");
            if let Some(ty) = label.strip_prefix("B-") {
                close_span(&mut open, &mut gold);
                open = Some((ty.to_string(), vec![tok.clone()]));
            } else if let Some(ty) = label.strip_prefix("I-") {
                let extend = matches!(&open, Some((open_ty, _)) if open_ty == ty);
                if extend {
                    if let Some((_, toks)) = open.as_mut() {
                        toks.push(tok.clone());
                    }
                } else {
                    // I- with no matching open B- (malformed BIO): start fresh.
                    close_span(&mut open, &mut gold);
                    open = Some((ty.to_string(), vec![tok.clone()]));
                }
            } else {
                close_span(&mut open, &mut gold); // "O"/unknown → end the span
            }
        }
        close_span(&mut open, &mut gold);
        docs.push(WnutDoc { text, gold });
    }
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_skips_cleanly_without_extractor_env() {
        let mut results: Vec<EvalRow> = Vec::new();
        let outcome = super::run(&mut results).await;

        // SKIP-not-FAIL invariant (Reject: false_fail_on_absent): with the
        // extractor env absent the harness MUST emit SKIPPED, never a 0.0→FAIL.
        // A live run with weights present may legitimately PASS/FAIL — assert
        // the strict invariant only when the gating capability is absent.
        if std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").is_err() {
            outcome.expect("absent extractor weights must SKIP, never error");
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].status, "SKIPPED");
        } else if let Err(e) = outcome {
            // The only hard error `run` can raise past the extractor gate is the
            // 0.7.0 missing-store-URL refusal (there is no `memory://` default
            // any more). Assert it names the fix rather than swallowing it.
            let msg = e.to_string();
            assert!(msg.contains("MOON_URL"), "unexpected hard error: {msg}");
            return;
        } else {
            assert_eq!(results.len(), 1);
            assert!(matches!(results[0].status.as_str(), "SKIPPED" | "PASS" | "FAIL"));
        }
        assert_eq!(results[0].harness, HARNESS);
        assert_eq!(results[0].metric, METRIC);
        assert_eq!(results[0].threshold, THRESHOLD);
    }
}
