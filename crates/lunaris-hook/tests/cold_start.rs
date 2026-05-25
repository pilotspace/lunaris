//! Cold-start latency gate (HOOK-06).
//!
//! Gate: p50 ≤ 50ms, p99 ≤ 150ms over 1000 deterministic envelopes through
//! the FULL pipeline (filter + scrub + dedupe + ingest) against an in-memory
//! SQLite instance (`memory://`).
//!
//! # Methodology
//!
//! - 10 warmup iterations (timings discarded) to warm SQLite page cache and
//!   compiled-regex caches (scrubber and filter policy).
//! - 990 measured iterations, each independently timed via `std::time::Instant`.
//! - Sort timings ascending; p50 = sorted[494], p99 = sorted[979].
//! - Assertion: both must be ≤ their budget. On breach: explicit panic with
//!   the measured values so CI log shows actionable numbers.
//!
//! # Why in-process (not subprocess)
//!
//! This test measures the PIPELINE latency — filter + scrub + dedupe + storage
//! write — not process spawn overhead. Running in-process via `lunaris_hook::run()`
//! against a shared `Arc<Lunaris>` removes OS process-spawn jitter from the
//! measurement, giving a stable signal for the ingest budget.
//!
//! # Runtime flavour
//!
//! `current_thread` matches the `lunaris-hook` binary's tokio runtime so the
//! measurement reflects the same executor the production binary uses.
//!
//! # Fixture
//!
//! `fixtures/replay_1000.json` is a committed NDJSON file with exactly 1000
//! lines, 250 each of PreToolUse/PostToolUse/Stop/SessionStart. Generated via:
//!
//! ```text
//! cargo run --example gen_replay_fixture -p lunaris-hook -- --count 1000 --seed 42 \
//!   > crates/lunaris-hook/tests/fixtures/replay_1000.json
//! ```
//!
//! # T-24-04-03 mitigation
//!
//! `memory://` creates an in-process ephemeral DB — no shared file, no SQLite
//! lock contention from parallel test runners. Pinned to `current_thread` to
//! prevent concurrent test subtasks racing the same pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lunaris::Lunaris;
use lunaris_core::{HlcClock, NoopEmbedder, Scope};
use lunaris_storage_embedded::EmbeddedStorage;

/// Fixture: 1000 NDJSON envelopes, 250 per kind (PreToolUse/PostToolUse/Stop/SessionStart).
/// Committed to the repo — deterministically generated via `gen_replay_fixture`.
const FIXTURE: &str = include_str!("fixtures/replay_1000.json");

/// Build a `Lunaris` handle backed by an in-memory SQLite database.
///
/// Uses `NoopEmbedder` so no GGUF weights are loaded — the hook pipeline is
/// embed-free at capture time (HOOK-06 budget invariant: no GGUF deps in
/// `lunaris-hook/Cargo.toml`).
async fn build_lunaris_in_memory() -> Lunaris {
    let storage = EmbeddedStorage::connect("memory://")
        .await
        .expect("EmbeddedStorage::connect(memory://) must succeed in cold_start test");
    let embedder = Arc::new(NoopEmbedder::new(768));
    let storage_arc: Arc<dyn lunaris_core::StoragePort> = Arc::new(storage);
    let clock = HlcClock::new(0);
    Lunaris::with_parts(storage_arc, embedder, clock)
}

/// Scope used for the latency gate — fixed string avoids git repo I/O.
fn gate_scope() -> Scope {
    Scope::new("cold-start-gate").expect("scope must be valid")
}

/// HOOK-06 latency CI gate.
///
/// p50 ≤ 50ms, p99 ≤ 150ms over 990 measured envelopes (10 warmup discarded).
/// If either budget is breached this test panics with the measured values so
/// CI logs show the exact regression. On CI hardware that consistently breaches
/// the budget add `#[ignore]` and supply a `milestones/v0.5-HOOK-LATENCY.json`
/// from a conforming developer machine.
#[tokio::test(flavor = "current_thread")]
async fn hook_pipeline_meets_latency_budget() {
    // Suppress tracing noise in test output — only WARN+ reaches stderr.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();

    let handle = build_lunaris_in_memory().await;
    let lunaris = Arc::new(handle);
    let scope = gate_scope();

    // Parse fixture lines.
    let lines: Vec<&str> = FIXTURE.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1000,
        "fixture must have exactly 1000 lines, got {}",
        lines.len()
    );

    // ── Warmup: first 10 envelopes ────────────────────────────────────────────
    //
    // Warms SQLite's WAL state machine, compiled-regex caches in scrubber and
    // filter policy, and the dedupe hash path. Timings are discarded.
    for line in lines.iter().take(10) {
        let bytes = line.as_bytes();
        let _ = lunaris_hook::run(bytes, scope.clone(), Arc::clone(&lunaris)).await;
    }

    // ── Measurement: remaining 990 envelopes ─────────────────────────────────
    let mut timings: Vec<Duration> = Vec::with_capacity(990);

    for line in lines.iter().skip(10) {
        let bytes = line.as_bytes();
        let t = Instant::now();
        let _ = lunaris_hook::run(bytes, scope.clone(), Arc::clone(&lunaris)).await;
        timings.push(t.elapsed());
    }

    assert_eq!(timings.len(), 990, "must have 990 timing samples");

    // ── Compute p50 / p99 ─────────────────────────────────────────────────────
    timings.sort_unstable();

    // p50 = index 494 (0-based) of 990 sorted samples  (floor(990 * 0.50) - 1).
    // p99 = index 979 (0-based) of 990 sorted samples  (floor(990 * 0.99) - 1).
    let p50 = timings[494];
    let p99 = timings[979];

    // Print evidence for CI log / operator review.
    println!(
        "HOOK-06 latency gate:\n  p50 = {:.3}ms (budget: ≤50ms)\n  p99 = {:.3}ms (budget: ≤150ms)",
        p50.as_secs_f64() * 1000.0,
        p99.as_secs_f64() * 1000.0,
    );

    // ── Budget assertions ─────────────────────────────────────────────────────
    assert!(
        p50 <= Duration::from_millis(50),
        "HOOK-06 budget violation: p50 = {:.3}ms (limit 50ms). \
         Investigate filter/scrub/dedupe path for regressions.",
        p50.as_secs_f64() * 1000.0,
    );
    assert!(
        p99 <= Duration::from_millis(150),
        "HOOK-06 budget violation: p99 = {:.3}ms (limit 150ms). \
         Investigate storage write path or regex compilation for regressions.",
        p99.as_secs_f64() * 1000.0,
    );

    println!(
        "HOOK-06 PASS: p50={:.3}ms p99={:.3}ms",
        p50.as_secs_f64() * 1000.0,
        p99.as_secs_f64() * 1000.0,
    );
}
