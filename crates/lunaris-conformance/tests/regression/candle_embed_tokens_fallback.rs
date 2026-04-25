//! Plan 14-03 Task 4 — Regression test for v0.1.1 bug #3:
//! **candle `embed_tokens` tensor-path fallback for SentenceTransformer layout**.
//!
//! ## Bug history
//!
//! `CandleEmbeddingGemma::new` (see
//! `crates/lunaris-embed/src/candle_gemma.rs:177-187`) originally
//! looked up the token-embedding matrix via the nested path
//! `vb.pp("model").pp("embed_tokens").get_unchecked("weight")`, which
//! matches HuggingFace's raw gemma3 safetensors layout. But the
//! SentenceTransformer-wrapped `google/embeddinggemma-300m` checkpoint
//! Google publishes flattens the path to `embed_tokens.weight` — so the
//! original loader panicked on first use with the published weights.
//!
//! The v0.1.1 fix is the `.or_else(|_| vb.pp("embed_tokens").get_unchecked("weight"))`
//! branch at line 181 — fall back to the flat path if the nested one
//! isn't present.
//!
//! See `.planning/milestones/v0.1.1-MILESTONE-AUDIT.md` and
//! `.planning/ROADMAP.md` Phase 14 block.
//!
//! ## Test oracle — positive path (`pg-lunaris` AND vanilla `postgres:16`)
//!
//! Invariant: `CandleEmbeddingGemma::new(CandleEmbeddingGemmaOpts::default())`
//! succeeds against a model cache whose safetensors file uses the
//! SentenceTransformer-flat layout (`embed_tokens.weight` at top level,
//! no `model.*` prefix). The published `google/embeddinggemma-300m`
//! checkpoint uses this layout — confirmed empirically via a hex dump
//! of the cached `model.safetensors` header (one `embed_tokens.weight`
//! key, no `model.embed_tokens.weight` entry).
//!
//! Two-tier skip discipline: the weights cache is not available on
//! every machine (the file is ~1.2 GiB). When the default cache path
//! (`~/.cache/lunaris/models/embedding-gemma-300m/`) has neither
//! `tokenizer.json` nor `model.safetensors`, the test short-circuits
//! to `Ok(())` — same pattern as the live-backend tests.
//!
//! ## Bug #3 is NOT Postgres-dependent
//!
//! This test is `#[cfg(feature = "pg-it")]`-gated only for matrix
//! consistency with the 3 PG-dependent regression tests (see
//! `tests/regression.rs` rustdoc). It does NOT require `PG_URL`. The
//! vanilla-pg-negative-matrix CI job in
//! `.github/workflows/integration.yml` treats this test's entry in
//! `EXPECTED_VANILLA_ERRORS` as `None` → asserts exit code 0, NOT a
//! stderr substring. If the test fails on vanilla pg:16, the CI job
//! correctly reports a real regression rather than a false negative.

#![cfg(feature = "pg-it")]

use std::path::PathBuf;

/// Per `EXPECTED_VANILLA_ERRORS[2]` in `tests/regression.rs`, this test
/// does NOT have a vanilla-pg expected error — it is expected to PASS on
/// both pg-lunaris AND vanilla pg:16. The const below documents that
/// explicitly so a future reader does not wonder why the constant is
/// absent; the workflow post-step treats `Option::None` as "assert exit 0".
///
/// (Empty value is intentional — this is a sentinel for "no substring
/// applies", not a substring to grep for. The YAML list in
/// `integration.yml` must mark this test as `expected_to_pass: true`.)
pub const EXPECTED_VANILLA_ERROR_NOT_APPLICABLE: &str = "";

#[tokio::test]
async fn candle_embed_tokens_fallback() -> anyhow::Result<()> {
    // Resolve the default cache path the same way `CandleEmbeddingGemmaOpts::default`
    // does (`dirs::cache_dir()/lunaris/models/embedding-gemma-300m/`).
    // If the required files are absent, skip — this is NOT a failure
    // (the invariant being verified is: IF the cache is present, loading
    // succeeds; the test does NOT download weights).
    let cache_root = match dirs::cache_dir() {
        Some(p) => p,
        None => {
            eprintln!("regression::candle_embed_tokens_fallback: SKIP (no user cache dir)");
            return Ok(());
        }
    };
    let model_path: PathBuf =
        cache_root.join("lunaris").join("models").join("embedding-gemma-300m");
    let tokenizer = model_path.join("tokenizer.json");
    let safetensors = model_path.join("model.safetensors");
    if !tokenizer.exists() || !safetensors.exists() {
        eprintln!(
            "regression::candle_embed_tokens_fallback: SKIP (weights not present at {}; \
             run `huggingface-cli download google/embeddinggemma-300m --local-dir <path>` to enable)",
            model_path.display()
        );
        return Ok(());
    }

    // The invariant: constructing the embedder against the SentenceTransformer-
    // layout safetensors succeeds. The v0.1.1 fallback at
    // `candle_gemma.rs:181` is what makes this work — the published
    // `google/embeddinggemma-300m` cache uses the flat `embed_tokens.weight`
    // path (verified empirically 2026-04-23: `python -c "safetensors header"`
    // shows exactly one matching key, `embed_tokens.weight`, not
    // `model.embed_tokens.weight`).
    let embedder = lunaris_embed::CandleEmbeddingGemma::new(
        lunaris_embed::CandleEmbeddingGemmaOpts::default(),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "CandleEmbeddingGemma::new failed against SentenceTransformer-layout \
             weights at {}: {e} — regression of the `model.embed_tokens.weight` \
             → `embed_tokens.weight` fallback (candle_gemma.rs:181)",
            model_path.display()
        )
    })?;

    // Minimal smoke — embed one short string to prove the forward path is
    // also not broken. Rank/dim assertions are the domain of the embedder's
    // own tests; here we only care that load + call did not panic.
    let out = lunaris_core::Embedder::embed_batch(&embedder, &["regression smoke"]).await?;
    assert_eq!(out.len(), 1, "embed_batch returned {} rows (expected 1)", out.len());
    assert_eq!(
        out[0].len(),
        lunaris_embed::candle_gemma::EMBEDDING_GEMMA_DIM,
        "embed_batch row width {} (expected {})",
        out[0].len(),
        lunaris_embed::candle_gemma::EMBEDDING_GEMMA_DIM
    );

    Ok(())
}
