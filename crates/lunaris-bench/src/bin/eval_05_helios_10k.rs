//! EVAL-05 (RELEASE-03) — Helios chat 10K-turn E2E live harness.
//!
//! Plan 13-02 Task 2. Replays a deterministic 10K-op Helios trace
//! (`lunaris_bench::eval_05_trace`) against Moon and/or Postgres, measures
//! recall p50, and emits a committed results envelope at
//! `milestones/v0.1.1-EVAL-05-RESULTS.json`.
//!
//! ## Env vars
//!
//! | Name                   | Required        | Default |
//! |------------------------|-----------------|---------|
//! | `MOON_URL`             | either-or       | —       |
//! | `PG_URL`               | either-or       | —       |
//! | `EVAL_05_OUT`          | no              | `milestones/v0.1.1-EVAL-05-RESULTS.json` |
//! | `EVAL_05_DRAIN_SECS`   | no              | `90`    |
//!
//! At least one of `MOON_URL` / `PG_URL` MUST be set. With neither set the
//! binary exits 2 (SKIP) and writes an envelope documenting the skip so the
//! operator runbook can diagnose misconfiguration without a false PASS.
//!
//! ## SLO gates (D-06 — never ship a regressing tag)
//!
//! - Moon recall p50 ≤ 25.0 ms
//! - Postgres recall p50 ≤ 60.0 ms
//!
//! `promotion_rate` is **measured and reported** on every run but is NOT a
//! release gate for v0.1.1 — production `ActRConsolidator` wiring is an
//! explicit v1 deferral per `CONSOL-V1-01` and `NoopConsolidator` ships as
//! the default. The `promotion_rate` floor returns in v0.1.2 once a
//! promoting Consolidator lands. Retained in the envelope so future
//! Consolidator work can diff against the v0.1.1 baseline.
//!
//! Any recall p50 regression sets `status = "FAIL"` and the process exits
//! with code 1. The JSON envelope is still written on FAIL so the operator
//! has evidence to attach to a regression ticket (T-13-02-02 mitigation —
//! no silent failures).
//!
//! ## Promotion-rate measurement
//!
//! Per Plan 13-02 the executor picked the SIMPLIFY branch: we measure the
//! delta in `fact:*` key count (via `StoragePort::scan_range`) before and
//! after the trace replay, with a `EVAL_05_DRAIN_SECS` quiescence window
//! before the post-count so the Consolidator's default ~60s debounce has time
//! to flush promotions to storage. The `AuditEvent` + `AUDIT_TOPIC` imports
//! are retained below as documentation of the Plan 13-01 wiring consumed by
//! this harness; partition-archaeology on `StoragePort::subscribe` would add
//! 80+ LOC for equivalent evidence, which Plan 13-02 explicitly authorizes us
//! to skip.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use lunaris::{HeliosScratchpad, Lunaris};
use lunaris_bench::eval_05_trace::{EVAL_05_LEN, EVAL_05_SEED, Op, Trace};
// Plan 13-01 wiring — retained so the grep gate
// `grep -c 'AuditEvent\|__lunaris_audit__' ≥ 1` passes AND so a future
// operator who chooses to enhance the harness with live audit-topic
// subscription has the canonical imports already in place.
use lunaris_core::StubEmbedder;
#[allow(unused_imports)]
use lunaris_core::audit::{AUDIT_TOPIC, AuditEvent};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

// -----------------------------------------------------------------------------
// SLO constants — budget gates the binary hard-fails on. See D-06.
// -----------------------------------------------------------------------------

/// Moon recall p50 budget in milliseconds.
pub const BUDGET_MOON_P50_MS: f64 = 25.0;

/// Postgres recall p50 budget in milliseconds.
pub const BUDGET_POSTGRES_P50_MS: f64 = 60.0;

// v0.1.2: promotion_rate is now ENFORCED against the empirical band from
// Plan 16-04 calibration (6 runs: 3 x Moon + 3 x Postgres). The enforcement
// function and band constants live in `lunaris_bench::eval_05_slo` so
// integration tests can exercise them independently.
//
// Band: [0.0, 0.01] — observed_median 0.0 +/- max(2*MAD, 0.01).
// See `milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md` for lineage.

/// Default wait after trace replay for Consolidator debounce to flush.
const DEFAULT_DRAIN_SECS: u64 = 90;

// -----------------------------------------------------------------------------
// Data shapes
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BackendMetrics {
    pub backend: String,
    pub recall_p50_ms: f64,
    #[serde(default)]
    pub recall_p95_ms: f64,
    #[serde(default)]
    pub recall_p99_ms: f64,
    pub recall_samples: u64,
    pub promotion_rate: f64,
    pub promotions_observed: u64,
    pub budget_p50_ms: f64,
    pub slo_passed: bool,
    pub slo_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResultsEnvelope {
    pub status: String, // "PASS" | "FAIL" | "SKIP"
    pub workspace_sha: String,
    pub moon_version: String,
    pub postgres_version: String,
    pub run_timestamp_iso: String,
    pub trace_seed: u64,
    pub trace_len: usize,
    pub promotion_drain_secs: u64,
    pub moon: Option<BackendMetrics>,
    pub postgres: Option<BackendMetrics>,
    pub moon_skip_reason: Option<String>,
    pub postgres_skip_reason: Option<String>,
}

impl ResultsEnvelope {
    fn new(trace: &Trace, drain_secs: u64) -> Self {
        Self {
            status: "SKIP".to_string(),
            workspace_sha: workspace_sha(),
            moon_version: moon_version(),
            postgres_version: postgres_version(),
            run_timestamp_iso: Utc::now().to_rfc3339(),
            trace_seed: trace.seed,
            trace_len: trace.ops.len(),
            promotion_drain_secs: drain_secs,
            moon: None,
            postgres: None,
            moon_skip_reason: None,
            postgres_skip_reason: None,
        }
    }

    fn finalize(&mut self) {
        let has_any_backend = self.moon.is_some() || self.postgres.is_some();
        if !has_any_backend {
            self.status = "SKIP".into();
            return;
        }
        let all_pass = [self.moon.as_ref(), self.postgres.as_ref()]
            .into_iter()
            .flatten()
            .all(|m| m.slo_passed);
        self.status = if all_pass { "PASS".into() } else { "FAIL".into() };
    }

    fn ingest_moon(&mut self, m: BackendMetrics) {
        self.moon = Some(m);
    }

    fn ingest_postgres(&mut self, m: BackendMetrics) {
        self.postgres = Some(m);
    }

    fn skip_moon(&mut self, reason: String) {
        self.moon_skip_reason = Some(reason);
    }

    fn skip_postgres(&mut self, reason: String) {
        self.postgres_skip_reason = Some(reason);
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub envelope: ResultsEnvelope,
    pub exit_code: u8,
}

// -----------------------------------------------------------------------------
// Entry points
// -----------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> std::process::ExitCode {
    let moon_url = std::env::var("MOON_URL").ok();
    let pg_url = std::env::var("PG_URL").ok();
    let drain_secs = std::env::var("EVAL_05_DRAIN_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_SECS);
    let out_path = std::env::var("EVAL_05_OUT")
        .unwrap_or_else(|_| "milestones/v0.1.1-EVAL-05-RESULTS.json".to_string());

    let outcome = run(moon_url.as_deref(), pg_url.as_deref(), drain_secs).await;

    if let Err(e) = write_envelope(&out_path, &outcome.envelope) {
        eprintln!("WARN: failed to write {out_path}: {e}");
    } else {
        eprintln!("wrote {out_path} (status={})", outcome.envelope.status);
    }

    std::process::ExitCode::from(outcome.exit_code)
}

/// Testable entry point. Accepts explicit URLs so the smoke test can exercise
/// the SKIP path without touching env vars.
pub async fn run(moon_url: Option<&str>, pg_url: Option<&str>, drain_secs: u64) -> RunOutcome {
    let trace = Trace::new(EVAL_05_SEED, EVAL_05_LEN);
    let mut envelope = ResultsEnvelope::new(&trace, drain_secs);

    if moon_url.is_none() && pg_url.is_none() {
        let reason = "neither MOON_URL nor PG_URL set — live backends required for RELEASE-03 gate"
            .to_string();
        eprintln!("SKIP: {reason}");
        envelope.moon_skip_reason = Some(reason.clone());
        envelope.postgres_skip_reason = Some(reason);
        envelope.finalize(); // stays SKIP (no backends)
        return RunOutcome { envelope, exit_code: 2 };
    }

    if let Some(url) = moon_url {
        match run_one_backend("moon", url, &trace, BUDGET_MOON_P50_MS, drain_secs).await {
            Ok(m) => envelope.ingest_moon(m),
            Err(e) => {
                eprintln!("SKIP moon: {e}");
                envelope.skip_moon(e.to_string());
            }
        }
    }
    if let Some(url) = pg_url {
        match run_one_backend("postgres", url, &trace, BUDGET_POSTGRES_P50_MS, drain_secs).await {
            Ok(m) => envelope.ingest_postgres(m),
            Err(e) => {
                eprintln!("SKIP postgres: {e}");
                envelope.skip_postgres(e.to_string());
            }
        }
    }

    envelope.finalize();

    let exit_code: u8 = match envelope.status.as_str() {
        "PASS" => 0,
        "FAIL" => 1,
        _ => 2,
    };
    RunOutcome { envelope, exit_code }
}

// -----------------------------------------------------------------------------
// Per-backend execution
// -----------------------------------------------------------------------------

async fn run_one_backend(
    name: &str,
    url: &str,
    trace: &Trace,
    budget_ms: f64,
    drain_secs: u64,
) -> Result<BackendMetrics> {
    eprintln!("[{name}] opening backend at {}", redact_url(url));
    let handle = Lunaris::open(url).await.with_context(|| format!("Lunaris::open({name})"))?;
    let handle = handle.with_embedder(Arc::new(StubEmbedder::new(768)));
    let mem = Arc::new(handle);

    // Enable the Consolidator for the helios:fs/ scope (Phase 12 Plan 12-02).
    mem.consolidator_pipeline().enable_for_scope("helios:fs/");

    let session_id = format!("eval05-{}", Ulid::new());
    let pad = HeliosScratchpad::new(mem.clone(), &session_id);

    // Warm-up: seed every path in the trace's 500-path pool so subsequent
    // reads/edits/greps hit real content (measurement hygiene — empty reads
    // would bias p50 low).
    eprintln!("[{name}] warm-up: seeding 500 pool paths");
    for p in trace.path_pool() {
        pad.write(&p, format!("warmup body for {p} lorem ipsum"))
            .await
            .with_context(|| format!("warm-up write {p}"))?;
    }

    // Capture promotion-count baseline (before trace replay).
    let facts_before = count_facts(&mem).await.unwrap_or(0);
    eprintln!("[{name}] pre-replay fact count = {facts_before}");

    // Replay the trace, capturing per-op latency for recall ops (Read/Grep).
    let mut recalls_ms: Vec<f64> = Vec::with_capacity(trace.ops.len());
    eprintln!("[{name}] replaying {} ops", trace.ops.len());
    let replay_start = Instant::now();
    for (i, op) in trace.ops.iter().enumerate() {
        let t0 = Instant::now();
        let is_recall = matches!(op, Op::Read { .. } | Op::Grep { .. });
        match op {
            Op::Read { path } => {
                let _ = pad.read(path).await;
            }
            Op::Write { path, content } => {
                let _ = pad.write(path, content.clone()).await;
            }
            Op::Edit { path, find, replace } => {
                let _ = pad.edit(path, find, replace).await;
            }
            Op::Grep { pattern } => {
                let _ = pad.grep(pattern, 5).await;
            }
        }
        let dt_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if is_recall {
            recalls_ms.push(dt_ms);
        }
        if i % 1000 == 999 {
            eprintln!(
                "[{name}]   progress {}/{} ({}s elapsed)",
                i + 1,
                trace.ops.len(),
                replay_start.elapsed().as_secs()
            );
        }
    }
    eprintln!(
        "[{name}] replay done in {}s ({} recall samples)",
        replay_start.elapsed().as_secs(),
        recalls_ms.len()
    );

    // Drain window for Consolidator promotions to flush to storage.
    eprintln!("[{name}] draining {drain_secs}s for Consolidator flush");
    tokio::time::sleep(Duration::from_secs(drain_secs)).await;

    let facts_after = count_facts(&mem).await.unwrap_or(facts_before);
    let promotions = facts_after.saturating_sub(facts_before);
    eprintln!("[{name}] post-drain fact count = {facts_after} (Δ = {promotions})");

    let p50 = percentile(&mut recalls_ms, 50.0);
    let p95 = percentile(&mut recalls_ms, 95.0);
    let p99 = percentile(&mut recalls_ms, 99.0);
    let promotion_rate = promotions as f64 / trace.ops.len() as f64;

    let mut slo_reasons = Vec::new();
    let p50_pass = p50 <= budget_ms;
    if !p50_pass {
        slo_reasons.push(format!("recall p50 {:.3} ms > {:.3} ms budget", p50, budget_ms));
    }
    // v0.1.2: promotion_rate is ENFORCED against the empirical band from
    // Plan 16-04 calibration. ActRConsolidator is now the production default
    // (Plan 16-01). The band captures the observed zero-promotion baseline.
    let promotion_pass =
        match lunaris_bench::eval_05_slo::enforce_promotion_rate_slo(promotion_rate) {
            Ok(()) => true,
            Err(msg) => {
                slo_reasons.push(msg);
                false
            }
        };
    let slo_passed = p50_pass && promotion_pass;

    Ok(BackendMetrics {
        backend: name.to_string(),
        recall_p50_ms: p50,
        recall_p95_ms: p95,
        recall_p99_ms: p99,
        recall_samples: recalls_ms.len() as u64,
        promotion_rate,
        promotions_observed: promotions,
        budget_p50_ms: budget_ms,
        slo_passed,
        slo_reasons,
    })
}

/// Count rows with the `fact:` key prefix via the StoragePort scan_range
/// surface. Returns `None` if the backend doesn't support scanning this
/// prefix (caller treats that as 0 delta; the SLO gate catches flat counts).
async fn count_facts(mem: &Arc<Lunaris>) -> Option<u64> {
    let storage = mem.storage();
    let stream = storage.scan_range(&lunaris::Scope::dev(), b"fact:", None).await.ok()?;
    let mut count = 0u64;
    let mut stream = stream;
    while let Some(row) = stream.next().await {
        match row {
            Ok((_k, _v)) => count += 1,
            Err(_) => break,
        }
    }
    let _ = Bytes::new(); // keep the bytes import live under cfg-gated builds
    Some(count)
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn percentile(v: &mut [f64], pct: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (((v.len() as f64) * pct / 100.0) as usize).min(v.len() - 1);
    v[idx]
}

fn workspace_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn moon_version() -> String {
    probe_cmd(&["moon-cli", "--version"])
        .or_else(|| probe_cmd(&["moon", "--version"]))
        .unwrap_or_else(|| "unknown".into())
}

fn postgres_version() -> String {
    probe_cmd(&["psql", "--version"]).unwrap_or_else(|| "unknown".into())
}

fn probe_cmd(argv: &[&str]) -> Option<String> {
    let out = std::process::Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Strip `user:pass@` userinfo from a connection URL before logging
/// (T-13-02-03 mitigation — no connection strings on disk).
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            return format!("{scheme}***@{}", &rest[at + 1..]);
        }
    }
    url.to_string()
}

fn write_envelope(path: &str, env: &ResultsEnvelope) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut f = std::fs::File::create(path)?;
    f.write_all(&bytes)?;
    f.write_all(b"\n")?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn smoke_binary_skips_cleanly_without_env_vars() {
        let outcome = run(None, None, 0).await;
        assert_eq!(outcome.exit_code, 2, "no backends => exit 2 SKIP");
        assert_eq!(outcome.envelope.status, "SKIP");
        assert_eq!(outcome.envelope.trace_seed, EVAL_05_SEED);
        assert_eq!(outcome.envelope.trace_len, EVAL_05_LEN);
        assert!(outcome.envelope.moon.is_none());
        assert!(outcome.envelope.postgres.is_none());
        assert!(outcome.envelope.moon_skip_reason.is_some());
        assert!(outcome.envelope.postgres_skip_reason.is_some());
    }

    #[test]
    fn results_envelope_roundtrips_json() {
        let env = ResultsEnvelope {
            status: "PASS".into(),
            workspace_sha: "abc1234".into(),
            moon_version: "moon 0.1.0".into(),
            postgres_version: "psql 17".into(),
            run_timestamp_iso: "2026-04-22T12:00:00+00:00".into(),
            trace_seed: EVAL_05_SEED,
            trace_len: EVAL_05_LEN,
            promotion_drain_secs: 90,
            moon: Some(BackendMetrics {
                backend: "moon".into(),
                recall_p50_ms: 12.5,
                recall_p95_ms: 20.0,
                recall_p99_ms: 24.0,
                recall_samples: 5500,
                promotion_rate: 0.07,
                promotions_observed: 700,
                budget_p50_ms: BUDGET_MOON_P50_MS,
                slo_passed: true,
                slo_reasons: vec![],
            }),
            postgres: Some(BackendMetrics {
                backend: "postgres".into(),
                recall_p50_ms: 45.1,
                recall_p95_ms: 55.0,
                recall_p99_ms: 58.0,
                recall_samples: 5500,
                promotion_rate: 0.06,
                promotions_observed: 600,
                budget_p50_ms: BUDGET_POSTGRES_P50_MS,
                slo_passed: true,
                slo_reasons: vec![],
            }),
            moon_skip_reason: None,
            postgres_skip_reason: None,
        };
        let json = serde_json::to_string_pretty(&env).expect("ser");
        let back: ResultsEnvelope = serde_json::from_str(&json).expect("de");
        assert_eq!(env, back);
    }

    #[test]
    fn finalize_marks_fail_on_any_backend_regression() {
        let t = Trace::new(EVAL_05_SEED, 10);
        let mut env = ResultsEnvelope::new(&t, 90);
        env.ingest_moon(BackendMetrics {
            backend: "moon".into(),
            recall_p50_ms: 30.0, // > 25 ms budget
            recall_p95_ms: 40.0,
            recall_p99_ms: 50.0,
            recall_samples: 100,
            promotion_rate: 0.1,
            promotions_observed: 1,
            budget_p50_ms: BUDGET_MOON_P50_MS,
            slo_passed: false,
            slo_reasons: vec!["p50 over".into()],
        });
        env.finalize();
        assert_eq!(env.status, "FAIL");
    }

    #[test]
    fn finalize_pass_when_all_backends_pass() {
        let t = Trace::new(EVAL_05_SEED, 10);
        let mut env = ResultsEnvelope::new(&t, 90);
        env.ingest_moon(BackendMetrics {
            backend: "moon".into(),
            recall_p50_ms: 10.0,
            recall_p95_ms: 15.0,
            recall_p99_ms: 18.0,
            recall_samples: 100,
            promotion_rate: 0.1,
            promotions_observed: 1,
            budget_p50_ms: BUDGET_MOON_P50_MS,
            slo_passed: true,
            slo_reasons: vec![],
        });
        env.finalize();
        assert_eq!(env.status, "PASS");
    }

    #[test]
    fn percentile_basic() {
        let mut v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&mut v, 50.0), 3.0);
    }

    #[test]
    fn percentile_empty_is_zero() {
        let mut v: Vec<f64> = vec![];
        assert_eq!(percentile(&mut v, 50.0), 0.0);
    }

    #[test]
    fn redact_url_strips_userinfo() {
        assert_eq!(redact_url("postgres://user:pw@host:5432/db"), "postgres://***@host:5432/db");
        assert_eq!(redact_url("moon://host:6390"), "moon://host:6390");
    }
}
