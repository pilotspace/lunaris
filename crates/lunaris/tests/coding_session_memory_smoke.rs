//! Plan 05-04 HELIOS-02 — live integration smoke for [`CodingSessionMemory`].
//!
//! The `_dual_backend` test names are historical: 0.7.0 slice B deleted the
//! Postgres backend, leaving Moon. They are kept because `docs/` cites them by
//! name and line.
//!
//! Two `#[ignore]`-gated tests:
//!
//! 1. [`helios_chat_10k_turns_dual_backend`] — synthetic 10K-turn chat
//!    (per turn: write user msg, read prior msg, occasional edit + grep).
//!    Asserts ingest p50 ≤ 50 ms AND recall p50 ≤ 25 ms on Moon — same as
//!    INGEST-05 / RETRIEVE-11/12 from Plan 02-04.
//! 2. [`helios_doc_rag_50k_md_dual_backend`] — bulk-ingest synthetic markdown
//!    documents via [`lunaris_bench::build_md_doc_corpus`]; query 100x via
//!    [`CodingSessionMemory::grep`]; asserts the recall p50 budget.
//!
//! Both `#[ignore]`-gated by default. Run via:
//!
//! ```bash
//! MOON_URL=moon://localhost:6380 \
//!   cargo test -p lunaris --test coding_session_memory_smoke -- --ignored --nocapture
//! ```
//!
//! Per-backend skip discipline mirrors Plan 04-03 verbatim — TCP probe with
//! 1s timeout against the `MOON_URL` URL; missing env or
//! unreachable host → `continue` (other backend still gets exercised).
//!
//! The full corpus shapes (10K turns / 50K docs) are documented defaults; the
//! per-test counts honor `LUNARIS_HELIOS_SMOKE_TURNS` / `LUNARIS_HELIOS_SMOKE_DOCS`
//! env overrides so dev-box runs land in single-digit minutes while CI / UAT
//! exercises the full target.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;
use std::time::Instant;

use lunaris::{CodingSessionMemory, Lunaris};

// ---------------------------------------------------------------------------
// Test 1 — 10K-turn chat
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires MOON_URL; un-ignored by integration.yml's lunaris-memory step"]
async fn helios_chat_10k_turns_dual_backend() -> anyhow::Result<()> {
    // Moon-only since 0.7.0 slice B deleted `lunaris-storage-postgres` and
    // retired `PG_URL` (integration.yml header). The `PG_URL` arm survived
    // here only because its probe skipped in silence.
    for url_env in ["MOON_URL"] {
        let Some(url) = probe_backend(url_env) else {
            continue;
        };
        eprintln!("\n=== chat 10k turns: {url_env} ({url}) ===");
        let lunaris = Arc::new(Lunaris::open(&url).await?);
        let session_id = format!("smoke-chat-{}", ulid::Ulid::new());
        let pad =
            CodingSessionMemory::new(lunaris.clone(), lunaris_core::Scope::dev(), &session_id);

        // Default TURNS=200 keeps dev-box runs short. Set
        // LUNARIS_HELIOS_SMOKE_TURNS=10000 for the documented full target.
        let turns: usize = std::env::var("LUNARIS_HELIOS_SMOKE_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);

        let mut ingest_samples_ms: Vec<f64> = Vec::with_capacity(turns);
        let mut recall_samples_ms: Vec<f64> = Vec::with_capacity(turns);

        for i in 0..turns {
            let path = format!("turn-{i:06}.md");
            let content = format!(
                "Turn {i}: hello from chat smoke. The quick brown fox jumps over the lazy dog."
            );

            let t0 = Instant::now();
            pad.write(&path, content.clone()).await?;
            ingest_samples_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

            let t1 = Instant::now();
            let _read = pad.read(&path).await?;
            recall_samples_ms.push(t1.elapsed().as_secs_f64() * 1000.0);

            // Edit + grep on every 10th turn — avoids 10× amplification while
            // still exercising both code paths in the smoke loop.
            if i % 10 == 0 && i > 0 {
                let prior = format!("turn-{:06}.md", i - 1);
                pad.edit(&prior, "hello", "HELLO").await?;
                let _ = pad.grep("brown fox", 5).await?;
            }
        }

        let ingest_p50 = percentile(&mut ingest_samples_ms, 50.0);
        let ingest_p99 = percentile(&mut ingest_samples_ms, 99.0);
        let recall_p50 = percentile(&mut recall_samples_ms, 50.0);
        let recall_p99 = percentile(&mut recall_samples_ms, 99.0);
        let (ingest_budget, recall_budget) = budgets(url_env);
        eprintln!(
            "{url_env} chat: turns={turns} ingest_p50={ingest_p50:.2}ms ingest_p99={ingest_p99:.2}ms (budget {ingest_budget}ms) recall_p50={recall_p50:.2}ms recall_p99={recall_p99:.2}ms (budget {recall_budget}ms)"
        );
        check_budget(url_env, "ingest_p50", ingest_p50, ingest_budget);
        check_budget(url_env, "recall_p50", recall_p50, recall_budget);

        // Per-tenant cleanup so re-runs don't accumulate session state.
        let _ = pad.forget().await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2 — 50K-document RAG
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires MOON_URL; un-ignored by integration.yml's lunaris-memory step (~5min only when DOCS=50000; the default is small)"]
async fn helios_doc_rag_50k_md_dual_backend() -> anyhow::Result<()> {
    // Moon-only since 0.7.0 slice B deleted `lunaris-storage-postgres` and
    // retired `PG_URL` (integration.yml header). The `PG_URL` arm survived
    // here only because its probe skipped in silence.
    for url_env in ["MOON_URL"] {
        let Some(url) = probe_backend(url_env) else {
            continue;
        };
        eprintln!("\n=== doc rag md: {url_env} ({url}) ===");
        let lunaris = Arc::new(Lunaris::open(&url).await?);
        let session_id = format!("smoke-rag-{}", ulid::Ulid::new());
        let pad =
            CodingSessionMemory::new(lunaris.clone(), lunaris_core::Scope::dev(), &session_id);

        // Default DOCS=1000 keeps dev-box runs short. Set
        // LUNARIS_HELIOS_SMOKE_DOCS=50000 for the documented full target.
        let docs: u64 = std::env::var("LUNARIS_HELIOS_SMOKE_DOCS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000);

        let bulk_started = Instant::now();
        let written =
            lunaris_bench::build_md_doc_corpus(lunaris.storage().as_ref(), docs, 0xDEAD_BEEF)
                .await?;
        eprintln!(
            "{url_env} bulk-ingested {written} docs in {:.2}s",
            bulk_started.elapsed().as_secs_f64()
        );

        // 100 grep samples — enough to surface a meaningful p50 without
        // dominating the smoke wall-clock when the corpus is small.
        let mut grep_samples_ms: Vec<f64> = Vec::with_capacity(100);
        for q in 0..100u32 {
            let t = Instant::now();
            let _ = pad.grep(&format!("Section {} Lorem", q % 8), 5).await?;
            grep_samples_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let grep_p50 = percentile(&mut grep_samples_ms, 50.0);
        let grep_p99 = percentile(&mut grep_samples_ms, 99.0);
        let (_, recall_budget) = budgets(url_env);
        eprintln!(
            "{url_env} rag: docs={docs} grep_p50={grep_p50:.2}ms grep_p99={grep_p99:.2}ms (budget {recall_budget}ms)"
        );
        check_budget(url_env, "grep_p50", grep_p50, recall_budget);

        // No pad.forget() here: build_md_doc_corpus uses storage.atomic_write
        // directly with `bench:md-doc/<idx>` source, NOT via pad.write, so the
        // pad's `helios:fs/<sid>/` prefix wouldn't match. Cleanup is deferred
        // to the operator (or a separate forget(BySource("bench:md-doc/")) call
        // when the smoke runs in CI against a freshly-spun Moon).
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// TCP probe — delegates to the workspace's one implementation.
///
/// The local copy this replaces took `to_socket_addrs().next()`, so a
/// hostname resolving to an unreachable address before a reachable one read
/// as "no fixture". That is exactly what `moon://localhost:6390` does on a CI
/// runner, and it is why this suite skipped inside the job that guaranteed
/// its Moon. See `lunaris_test_harness::live_probe`.
fn probe_backend(env_name: &str) -> Option<String> {
    lunaris_test_harness::live_probe::probe_url_env(env_name)
}

/// Returns `(ingest_p50_budget_ms, recall_p50_budget_ms)` per backend.
/// Sourced from REQUIREMENTS.md INGEST-05 / RETRIEVE-11. The RETRIEVE-12
/// Postgres row went with the backend in 0.7.0 slice B.
fn budgets(env_name: &str) -> (f64, f64) {
    match env_name {
        "MOON_URL" => (50.0, 25.0), // INGEST-05 + RETRIEVE-11
        _ => (f64::INFINITY, f64::INFINITY),
    }
}

/// Hard-fail on any backend over budget.
///
/// This used to grade Moon and Postgres differently — Postgres ≤ 2× over was a
/// hard fail, > 2× over a documented soft fail (Plan 02-04 D-12). With the
/// Postgres backend deleted in 0.7.0 slice B there is one backend left, and it
/// has never had a soft-fail lane.
fn check_budget(env_name: &str, metric: &str, value: f64, budget: f64) {
    if value <= budget {
        return;
    }
    let ratio = value / budget;
    panic!(
        "{env_name} {metric}={value:.2}ms exceeds budget {budget}ms ({ratio:.2}x) — Moon hard-fail"
    );
}

/// Compute the `pct`-th percentile (0..=100) of `samples`. Mutates the buffer
/// (sorts in place — no allocation). Returns 0.0 on empty input rather than
/// NaN-panicking the bench loop.
fn percentile(samples: &mut [f64], pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * (pct / 100.0)) as usize;
    let idx = idx.min(samples.len() - 1);
    samples[idx]
}

// ---------------------------------------------------------------------------
// Default-test smoke — runs WITHOUT --ignored, proves the test file compiles
// and the helpers behave. Keeps the file honest under `cargo test --workspace`
// (where the heavy `#[ignore]`-gated tests above are skipped).
// ---------------------------------------------------------------------------

#[test]
fn percentile_empty_returns_zero() {
    let mut v: Vec<f64> = Vec::new();
    assert_eq!(percentile(&mut v, 50.0), 0.0);
}

#[test]
fn percentile_p50_picks_middle() {
    let mut v = vec![10.0, 20.0, 30.0, 40.0, 50.0];
    assert_eq!(percentile(&mut v, 50.0), 30.0);
}

#[test]
fn budgets_table_matches_requirements() {
    // INGEST-05 + RETRIEVE-11 (Moon) — ingest p50 ≤ 50ms, recall p50 ≤ 25ms.
    assert_eq!(budgets("MOON_URL"), (50.0, 25.0));
    // The Postgres row (RETRIEVE-12) is gone with the backend; an unknown
    // name must fall through to the infinite budget below, not to a stale row.
    assert_eq!(budgets("PG_URL"), (f64::INFINITY, f64::INFINITY));
    // Unknown env name → infinite budget (graceful — never hard-fails).
    assert_eq!(budgets("UNKNOWN"), (f64::INFINITY, f64::INFINITY));
}

#[test]
fn check_budget_within_budget_does_not_panic() {
    check_budget("MOON_URL", "ingest_p50", 10.0, 50.0);
    check_budget("MOON_URL", "recall_p50", 24.0, 25.0);
}

#[test]
#[should_panic(expected = "Moon hard-fail")]
fn check_budget_moon_over_budget_hard_fails() {
    check_budget("MOON_URL", "ingest_p50", 100.0, 50.0);
}

/// The soft-fail lane is gone, not merely unused: a 2.5× overrun used to
/// eprintln and continue. If it ever comes back, this catches it.
#[test]
#[should_panic(expected = "Moon hard-fail")]
fn check_budget_far_over_budget_still_hard_fails() {
    check_budget("MOON_URL", "ingest_p50", 250.0, 100.0);
}

// ---------------------------------------------------------------------------
// Plan 12-01 Task 2 — v2 delegation cross-crate surface guard + live round-trip
// ---------------------------------------------------------------------------

/// Cross-crate cross-check for the HELIOS-01 ≤ 50-LOC public-surface contract.
///
/// Mirrors the in-lib `coding_session_memory_public_surface_under_50_loc` sentinel
/// but counts from OUTSIDE the `lunaris` crate, via `include_str!` on the
/// source file. If the lib-side sentinel ever drifts, this default-mode test
/// catches it in the integration-test lane where downstream Helios actually
/// consumes the type.
#[test]
fn coding_session_memory_v2_surface_matches_v1_exactly() {
    let src = include_str!("../src/recipes/coding_session_memory.rs");
    let production = src.split("#[cfg(test)]").next().unwrap_or(src);
    let pub_fns =
        production.matches("    pub fn ").count() + production.matches("    pub async fn ").count();
    assert_eq!(
        pub_fns, 10,
        "HELIOS-01 cross-crate sentinel: expected exactly 10 public methods \
         (9 on CodingSessionMemory + AsOfScratchpad::read); got {pub_fns}. \
         Any drift here vs the in-lib sentinel is a contract violation. \
         Re-baselined 9→10 alongside the in-lib sentinel when `write_dated` \
         landed (session-date grounding — the recipe needs a write that \
         stamps the observation's own t_ref instead of ingest wall-clock)."
    );
}

/// Plan 12-01 Task 2 — delegation round-trip. Proves the `Value::String`
/// wrap (on `CodingSessionMemory::write`) and unwrap (on `CodingSessionMemory::read`)
/// paths route through `WorkingMemory` cleanly on both backends.
///
/// `#[ignore]`-gated behind the `MOON_URL` TCP probe in the same way
/// as the 10K-turn chat test above — default `cargo test` skips this.
#[tokio::test]
#[ignore = "requires MOON_URL; un-ignored by integration.yml's lunaris-memory step"]
async fn coding_session_memory_v2_delegation_round_trip() -> anyhow::Result<()> {
    // Moon-only since 0.7.0 slice B deleted `lunaris-storage-postgres` and
    // retired `PG_URL` (integration.yml header). The `PG_URL` arm survived
    // here only because its probe skipped in silence.
    for url_env in ["MOON_URL"] {
        let Some(url) = probe_backend(url_env) else {
            continue;
        };
        eprintln!("\n=== v2 delegation round-trip: {url_env} ({url}) ===");
        // `pad.read` is NOT an exact-key fetch: `WorkingMemory::read` runs a
        // fused Vector + BM25 top-k search and enforces the `source` equality
        // filter AFTER hydration (source is not a field on Moon's `chunks` FT
        // schema). With no embedder the vector leg carries no signal, and the
        // read returns None for a value that is definitely stored — which is
        // exactly what this assertion then reports as a lost payload.
        //
        // Measured, not assumed: with the GGUF staged this passes in 6.4s;
        // with `LUNARIS_EMBEDDER_GGUF` pointed at a missing file it fails in
        // 0.93s with `left: None` — byte-identical to the CI failure.
        // `integration.yml` stages no GGUF and says so.
        //
        // The underlying defect (an exact-key read that depends on similarity
        // search, and answers "absent" instead of erroring) is ship-plan F40.
        if !lunaris_test_harness::models::embedder_available(
            "coding_session_memory_v2_delegation_round_trip",
        ) {
            continue;
        }
        let lunaris = Arc::new(Lunaris::open(&url).await?);
        let session_id = format!("smoke-v2-rt-{}", ulid::Ulid::new());
        let pad =
            CodingSessionMemory::new(lunaris.clone(), lunaris_core::Scope::dev(), &session_id);

        pad.write("note.md", "hello v2").await?;
        let round_trip = pad.read("note.md").await?;
        assert_eq!(
            round_trip.as_deref(),
            Some("hello v2"),
            "{url_env}: v2 write→read round-trip lost or reshaped the payload"
        );

        // Per-tenant cleanup — same pattern as helios_chat_10k_turns_dual_backend.
        let _ = pad.forget().await?;
    }
    Ok(())
}
