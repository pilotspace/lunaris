//! P1 correctness panel for `lunaris-embed-native`.
//!
//! Implements POST-VERIFY-PLAN.md categories §3 (Vietnamese), §4 (code),
//! §6.2 (Send+Sync static), §10 (cross-lingual retrieval). Strictly additive
//! to `numerical_equivalence.rs` — same env-var-skip pattern, same feature
//! gate (`embedder-it`).
//!
//! ## Run
//!
//! ```bash
//! export SNAP=$HOME/.cache/lunaris/spike/granite-r2/models--ibm-granite--granite-embedding-311m-multilingual-r2/snapshots/dba7b0ee9d789f330fecfb85df57699f9e7d9c42
//! GRANITE_R2_WEIGHTS_PATH=$SNAP/model.safetensors \
//! GRANITE_R2_TOKENIZER_PATH=$SNAP/tokenizer.json \
//! GRANITE_R2_CONFIG_PATH=$SNAP/config.json \
//! cargo test -p lunaris-embed-native --features embedder-it --release \
//!     --test p1_correctness -- --nocapture
//! ```
//!
//! The single test loads `NativeEmbedder` once, then walks three JSON fixtures.
//! Each assertion is appended to `Vec<TestResult>`; the table is printed at the
//! end, and the test panics ONLY after the summary so first-failure diagnostics
//! always include the full category breakdown.

#![cfg(feature = "embedder-it")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candle_core::Device;
use lunaris_core::Embedder;
use lunaris_embed_native::{GRANITE_R2_DIM, NativeEmbedder, NativeEmbedderOpts};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Compile-time trait conformance (POST-VERIFY-PLAN §6.1, §6.2).
// These functions are never called at runtime — they exist purely so the
// type system enforces the invariant. If a future change weakens the trait
// (e.g. drops `Send` or `Sync`) this test crate FAILS TO COMPILE — the
// strongest possible tripwire.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn assert_send_sync<T: Send + Sync>() {}

#[allow(dead_code)]
fn _static_trait_asserts(e: NativeEmbedder) {
    // why: Arc<NativeEmbedder> must be share-able across tokio tasks —
    // the core idiom for plugging the embedder into Lunaris's retrieval DSL.
    assert_send_sync::<Arc<NativeEmbedder>>();
    // why: NativeEmbedder itself is also Send+Sync (Arc<Inner> internally).
    assert_send_sync::<NativeEmbedder>();
    // why: §6.1 — Box<dyn Embedder> must be a valid trait object so the
    // public DI surface (`Box<dyn Embedder>`) compiles. The existing src/
    // unit test exercises Arc<dyn Embedder>; this is the Box<> twin.
    let _: Box<dyn Embedder> = Box::new(e);
}

// ---------------------------------------------------------------------------
// Fixture types.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ViPanel {
    schema_version: u32,
    prompts: Vec<ViPromptRow>,
}

#[derive(Debug, Deserialize)]
struct ViPromptRow {
    id: String,
    category: String,
    text: String,
    variant: String,
    assertion: PairAssertion,
}

#[derive(Debug, Deserialize)]
struct CodePanel {
    schema_version: u32,
    prompts: Vec<CodePromptRow>,
}

#[derive(Debug, Deserialize)]
struct CodePromptRow {
    id: String,
    category: String,
    text: String,
    variant: String,
    assertion: PairAssertion,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // names mirror the JSON fixture tag values (cos_gt / cos_lt / cos_between)
enum PairAssertion {
    CosGt { threshold: f64, rationale: String },
    CosLt { threshold: f64, rationale: String },
    CosBetween { lo: f64, hi: f64, rationale: String },
}

#[derive(Debug, Deserialize)]
struct CrossLingualPanel {
    schema_version: u32,
    corpus: Vec<CorpusDoc>,
    queries: Vec<QueryRow>,
}

#[derive(Debug, Deserialize)]
struct CorpusDoc {
    id: String,
    #[allow(dead_code)]
    lang: String,
    #[allow(dead_code)]
    topic: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct QueryRow {
    id: String,
    #[allow(dead_code)]
    lang: String,
    text: String,
    ground_truth_top3: Vec<String>,
}

// ---------------------------------------------------------------------------
// Result accumulator.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TestResult {
    category: String,
    prompt_id: String,
    passed: bool,
    detail: String,
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

fn load_json<T: for<'de> Deserialize<'de>>(p: &Path) -> T {
    let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
}

/// Cosine similarity for unit-norm vectors == dot product, but we don't
/// trust the upstream is exactly unit-norm at f64 precision. Use the safe
/// general form. f64 throughout to keep accumulated drift below assertion
/// tolerances.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "cosine: length mismatch");
    let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = *x as f64;
        let yf = *y as f64;
        dot += xf * yf;
        na += xf * xf;
        nb += yf * yf;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 { 0.0 } else { (dot / denom).clamp(-1.0, 1.0) }
}

fn check_pair(cos: f64, a: &PairAssertion) -> (bool, String) {
    match a {
        PairAssertion::CosGt { threshold, rationale } => {
            let ok = cos > *threshold;
            (ok, format!("cos={cos:.4} > {threshold:.4} ({rationale})"))
        }
        PairAssertion::CosLt { threshold, rationale } => {
            let ok = cos < *threshold;
            (ok, format!("cos={cos:.4} < {threshold:.4} ({rationale})"))
        }
        PairAssertion::CosBetween { lo, hi, rationale } => {
            let ok = cos >= *lo && cos <= *hi;
            (ok, format!("cos={cos:.4} ∈ [{lo:.4}, {hi:.4}] ({rationale})"))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers — env-var skip pattern, mirrored from numerical_equivalence.rs.
// ---------------------------------------------------------------------------

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

async fn embed_one(embedder: &NativeEmbedder, s: &str) -> Vec<f32> {
    let v = embedder.embed_batch(&[s]).await.expect("embed single");
    v.into_iter().next().expect("at least one row")
}

async fn embed_many(embedder: &NativeEmbedder, prompts: &[&str]) -> Vec<Vec<f32>> {
    let mut out = Vec::with_capacity(prompts.len());
    for chunk in prompts.chunks(8) {
        let batch = embedder.embed_batch(chunk).await.expect("embed_batch");
        out.extend(batch);
    }
    out
}

// ---------------------------------------------------------------------------
// THE test.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p1_correctness_panel() {
    // -- env-var skip (same convention as numerical_equivalence.rs) --
    let weights = match env_path("GRANITE_R2_WEIGHTS_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] GRANITE_R2_WEIGHTS_PATH unset");
            return;
        }
    };
    let tokenizer = match env_path("GRANITE_R2_TOKENIZER_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] GRANITE_R2_TOKENIZER_PATH unset");
            return;
        }
    };
    let config = match env_path("GRANITE_R2_CONFIG_PATH") {
        Some(p) => p,
        None => {
            eprintln!("[skip] GRANITE_R2_CONFIG_PATH unset");
            return;
        }
    };

    // -- load fixtures up front so JSON errors fail before model load --
    let vi: ViPanel = load_json(&fixture_path("vi_panel.json"));
    assert_eq!(vi.schema_version, 1);
    assert_eq!(vi.prompts.len(), 20, "vi_panel.json must have exactly 20 prompts");

    let code: CodePanel = load_json(&fixture_path("code_panel.json"));
    assert_eq!(code.schema_version, 1);
    assert_eq!(code.prompts.len(), 15, "code_panel.json must have exactly 15 prompts");

    let xling: CrossLingualPanel = load_json(&fixture_path("cross_lingual_panel.json"));
    assert_eq!(xling.schema_version, 1);
    assert_eq!(xling.corpus.len(), 50, "cross_lingual_panel.json must have exactly 50 docs");
    assert_eq!(xling.queries.len(), 10, "cross_lingual_panel.json must have exactly 10 queries");

    // -- load embedder once (heavy: ~5s + ~1.24 GB RSS) --
    let embedder = NativeEmbedder::open(NativeEmbedderOpts {
        weights_path: weights,
        tokenizer_path: tokenizer,
        config_path: config,
        device: Device::Cpu,
    })
    .expect("NativeEmbedder::open");
    assert_eq!(embedder.dim(), GRANITE_R2_DIM);

    let mut results: Vec<TestResult> = Vec::new();

    // ===================================================================
    // Category §3 — Vietnamese correctness
    // ===================================================================
    eprintln!("\n[it] === §3 Vietnamese panel ===");
    for row in &vi.prompts {
        // why: each VI pair tests a specific tokenizer/model failure mode
        // (diacritic strip, NFC↔NFD divergence, tone collapse, cross-lingual).
        let a = embed_one(&embedder, &row.text).await;
        let b = embed_one(&embedder, &row.variant).await;
        let cos = cosine(&a, &b);
        let (ok, detail) = check_pair(cos, &row.assertion);
        results.push(TestResult {
            category: format!("vi/{}", row.category),
            prompt_id: row.id.clone(),
            passed: ok,
            detail,
        });
    }

    // ===================================================================
    // Category §4 — Code embeddings
    // ===================================================================
    eprintln!("[it] === §4 Code panel ===");
    for row in &code.prompts {
        // why: code panel pairs guard against syntactic-shape over-fit
        // (same-shape pairs land >0.99), cross-lang code blindness
        // (Python↔Rust same algo <0.7), prose↔code collapse, and
        // VI-comment domination of the embedding.
        let a = embed_one(&embedder, &row.text).await;
        let b = embed_one(&embedder, &row.variant).await;
        let cos = cosine(&a, &b);
        let (ok, detail) = check_pair(cos, &row.assertion);
        results.push(TestResult {
            category: format!("code/{}", row.category),
            prompt_id: row.id.clone(),
            passed: ok,
            detail,
        });
    }

    // ===================================================================
    // Category §10 — Cross-lingual retrieval realism
    // ===================================================================
    eprintln!("[it] === §10 Cross-lingual retrieval ===");
    let corpus_refs: Vec<&str> = xling.corpus.iter().map(|d| d.text.as_str()).collect();
    let corpus_vecs = embed_many(&embedder, &corpus_refs).await;
    assert_eq!(corpus_vecs.len(), xling.corpus.len());

    for q in &xling.queries {
        let qv = embed_one(&embedder, &q.text).await;
        let mut scored: Vec<(usize, f64)> =
            corpus_vecs.iter().enumerate().map(|(i, doc)| (i, cosine(&qv, doc))).collect();
        // why: descending cosine — we want the closest docs to the query.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let top3_ids: Vec<&str> =
            scored.iter().take(3).map(|(i, _)| xling.corpus[*i].id.as_str()).collect();

        let hits = q
            .ground_truth_top3
            .iter()
            .filter(|gt| top3_ids.contains(&gt.as_str()))
            .count();

        // why: gate is ≥2 of 3 ground-truth docs in top-3. Strict enough to
        // catch a broken multilingual model; lenient enough to allow one
        // model-preference quirk. Below this == the granite-r2 multilingual
        // training did not produce useful retrieval (the whole premise of
        // picking it over EmbeddingGemma).
        let ok = hits >= 2;
        let detail = format!(
            "top3={:?} gt={:?} hits={hits}/3 (top1_cos={:.4})",
            top3_ids, q.ground_truth_top3, scored[0].1
        );
        results.push(TestResult {
            category: format!("xling/{}", q.lang),
            prompt_id: q.id.clone(),
            passed: ok,
            detail,
        });
    }

    // ===================================================================
    // Summary table — printed BEFORE the panic so first-failure runs
    // surface full category breakdowns.
    // ===================================================================
    print_summary(&results);

    let failures: Vec<&TestResult> = results.iter().filter(|r| !r.passed).collect();
    assert!(
        failures.is_empty(),
        "P1 panel: {} of {} assertions failed; see summary above",
        failures.len(),
        results.len()
    );
}

fn print_summary(results: &[TestResult]) {
    use std::collections::BTreeMap;
    let mut per_cat: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for r in results {
        let e = per_cat.entry(r.category.clone()).or_insert((0, 0));
        if r.passed {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }

    eprintln!("\n[it] ====== P1 SUMMARY ======");
    eprintln!("[it] {:<28} {:>6} {:>6} {:>6}", "category", "pass", "fail", "total");
    for (cat, (pass, fail)) in &per_cat {
        let total = pass + fail;
        eprintln!("[it] {cat:<28} {pass:>6} {fail:>6} {total:>6}");
    }
    let total_pass: u32 = per_cat.values().map(|(p, _)| *p).sum();
    let total_fail: u32 = per_cat.values().map(|(_, f)| *f).sum();
    eprintln!(
        "[it] {:<28} {:>6} {:>6} {:>6}",
        "TOTAL",
        total_pass,
        total_fail,
        total_pass + total_fail
    );

    if total_fail > 0 {
        eprintln!("\n[it] ---- failures ----");
        for r in results.iter().filter(|r| !r.passed) {
            eprintln!("[it] FAIL  {:<24} {}  ::  {}", r.category, r.prompt_id, r.detail);
        }
    }
}
