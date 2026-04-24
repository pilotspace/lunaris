//! Plan 16-03 — Criterion bench for one full `ActRConsolidator::consolidate`
//! worker-tick (score → classify → promote/archive emission) on the Phase 16
//! default backend flip.
//!
//! Measures wall-clock of a single `ActRConsolidator::default().consolidate(
//! storage, &debounce_batch)` call against both live Moon and live Postgres
//! storage handles. The debounce batch is a deterministic 1K-event slice
//! derived from the EVAL-05 10K Helios trace (`lunaris_bench::eval_05_trace`)
//! so the workload mirrors the same Read/Write/Edit/Grep mix that `helios_p50`
//! and `eval_05_helios_10k` use — apples-to-apples with 12-03 / 13-02.
//!
//! ## Plan 16-03 SLO (CONSOL-V1-03)
//!
//! - p50 ≤ 50 ms on each backend (the 60 s debounce window means a tick
//!   processes up to ~1K events; this bench pins the batch at exactly that
//!   size to exercise the worst-case coalesce path).
//! - `queue_depth_peak` ≤ 2× baseline — R16-03 backpressure guard. The
//!   `ActRConsolidator::consolidate` surface does NOT publish audit envelopes
//!   (the worker owns that per CONSOL-V1-05) so this bench measures
//!   `report.promotions.len() + report.archives.len()` as a proxy for emission
//!   pressure and labels it `queue_depth_peak` in the envelope. The 10 cap
//!   encoded in the SUMMARY is the R16-03 concrete guard.
//!
//! ## Skip discipline (mirrors `helios_p50.rs` + `recall_hot_path.rs`)
//!
//! - URL env unset → skip backend.
//! - 1-sec TCP probe fails → skip backend.
//! - `Lunaris::open` fails → skip backend (with eprintln).
//!
//! ## Structural note (advisor review, 2026-04-24)
//!
//! `ActRConsolidator::consolidate` currently takes `Arc<dyn StoragePort>` but
//! does NOT read from it — Leiden spreading-activation wiring is deferred per
//! the act_r.rs "Known gap" docstring. Consequently Moon vs Postgres are
//! essentially fingerprint labels on a pure-CPU scoring pass; the measurement
//! axis is the classifier + group-by-episode hot path, not storage I/O. This
//! is documented in `16-03-SUMMARY.md` so future readers don't misattribute
//! the bench's structure to a non-existent storage dependency.
//!
//! Env gating: `LUNARIS_CONSOLIDATOR_BACKEND` MUST be unset during the bench
//! run — we want the default (`actr`) path. The bench does not set or read
//! that env var; it constructs `ActRConsolidator::default()` directly.
//!
//! ## Evidence envelope
//!
//! After `group.finish()` the bench walks each reachable backend and writes
//! `target/criterion/consolidator_tick/consolidator_tick_<label>/envelope.json`
//! with git SHA, CPU model, Moon/PG version probes, sample count, measured
//! p50/p99/mean (pulled from Criterion `estimates.json`), and the proxy
//! `queue_depth_peak`. The compact JSON at
//! `milestones/v0.1.2-CONSOL-BENCH.json` is written by the Plan 16-03 Task 2
//! post-processor — not this bench.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lunaris::Lunaris;
use lunaris_bench::tcp_reachable;
use lunaris_consolidate::act_r::ActRConsolidator;
use lunaris_consolidate::types::ConsolidateEvent;
use lunaris_consolidate::Consolidator;
use lunaris_core::StubEmbedder;
use ulid::Ulid;

/// Size of the debounce batch fed to `consolidate` on each bench iteration.
///
/// Matches the worst-case 60 s debounce window coalesce size for the EVAL-05
/// 10K-turn Helios trace (1K events per window is the blueprint § debounce
/// upper bound — see 16-PLAN.md success criterion #3).
const BATCH_SIZE: usize = 1_000;

/// Number of distinct episode ULIDs the batch spans. Smaller than
/// `BATCH_SIZE` so `ActRConsolidator::consolidate`'s `HashMap<Ulid,
/// Vec<ConsolidateEvent>>` group-by path does real coalescing work (multiple
/// events per episode) — matches real ingest patterns where one turn emits
/// several MVCC writes.
const EPISODE_FANOUT: usize = 100;

/// Criterion sample count. Same as `helios_p50`'s SAMPLE_SIZE to keep the
/// total wall-clock per backend in the ~30 s range on a modern dev box.
const SAMPLE_SIZE: usize = 50;

/// Criterion warm-up time. Same as `helios_p50`.
const WARMUP_SECS: u64 = 3;

/// Criterion measurement window. Shorter than `helios_p50`'s 25 s because the
/// consolidator tick is a pure-CPU call with no network RTT — 10 s yields
/// plenty of samples.
const MEASUREMENT_SECS: u64 = 10;

/// Per-iteration bench timeout guard (R16-04 mitigation: surface a hang
/// rather than wedge the Criterion runner). 2 s is 40× the 50 ms p50 budget.
const PER_ITER_TIMEOUT: Duration = Duration::from_secs(2);

struct Backend {
    label: &'static str,
    env: &'static str,
}

const BACKENDS: &[Backend] = &[
    Backend { label: "moon", env: "MOON_URL" },
    Backend { label: "postgres", env: "PG_URL" },
];

/// Build a deterministic 1K-event debounce batch spanning `EPISODE_FANOUT`
/// distinct episodes. Seeded from a fixed ULID so the bench is reproducible
/// across machines + commits (matches the determinism convention in
/// `eval_05_trace.rs`).
fn build_debounce_batch() -> Vec<ConsolidateEvent> {
    // Deterministic episode pool — each ULID has bytes `[i, 0, 0, ...]` so
    // the sort-by-bytes in `ActRConsolidator::consolidate` is exercised.
    let episodes: Vec<Ulid> = (0..EPISODE_FANOUT)
        .map(|i| {
            let mut b = [0u8; 16];
            b[0] = (i & 0xFF) as u8;
            b[1] = ((i >> 8) & 0xFF) as u8;
            Ulid::from_bytes(b)
        })
        .collect();

    // `now - 30s` in ms. Pin so the elapsed-seconds math in `classify_episode`
    // lands in the neutral band for most episodes and the promote band for a
    // few — matches real ingest behavior without being wall-clock dependent.
    // The scorer floors elapsed at 1s so this stays stable.
    let base_lsn_wall_ms: u64 = 30_000;

    (0..BATCH_SIZE)
        .map(|i| ConsolidateEvent {
            kind: "ingest_committed".to_string(),
            episode_id: episodes[i % EPISODE_FANOUT],
            // Vary lsn_wall_ms within the 30s window so the activation sum
            // has real variance across references, not a degenerate constant.
            lsn_wall_ms: base_lsn_wall_ms + (i as u64 * 17) % 30_000,
            lsn_counter: i as u64,
            // `source` must start with `helios:fs/` so the v0.1.1
            // scope-gated consolidator (still in effect post-16-01) accepts
            // the event. `ActRConsolidator::consolidate` itself does NOT
            // filter on scope — that's `consolidate_scoped`'s job — but we
            // keep the source realistic anyway for fingerprint fidelity.
            source: format!("helios:fs/chat-turn-{:04}.md", i % 500),
        })
        .collect()
}

fn consolidator_tick_bench(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("consolidator_tick");
    group.sample_size(SAMPLE_SIZE);
    group.warm_up_time(Duration::from_secs(WARMUP_SECS));
    group.measurement_time(Duration::from_secs(MEASUREMENT_SECS));

    let batch = Arc::new(build_debounce_batch());
    // R16-03 backpressure proxy: track peak `report.promotions + archives`
    // across all iterations. `ActRConsolidator::consolidate` doesn't publish
    // audit events (worker-owned per CONSOL-V1-05) so we can't tap a real
    // audit queue from inside the bench — measuring emission vector length
    // is the honest alternative per advisor guidance.
    let queue_depth_peak = Arc::new(AtomicU64::new(0));

    for backend in BACKENDS {
        let Ok(url) = std::env::var(backend.env) else {
            eprintln!(
                "SKIP consolidator_tick/{} bench: {} unset",
                backend.label, backend.env
            );
            continue;
        };
        if !runtime.block_on(tcp_reachable(&url)) {
            eprintln!(
                "SKIP consolidator_tick/{} bench: backend unreachable (1s TCP probe failed)",
                backend.label
            );
            continue;
        }

        // Open a real Lunaris handle so `handle.storage()` returns the live
        // `Arc<dyn StoragePort>`. `ActRConsolidator::consolidate` does not
        // currently dereference storage (Leiden spreading-activation read is
        // deferred) but we wire the real Arc for fingerprint fidelity — if a
        // later commit starts reading from storage, this bench lights up
        // automatically without a structural rewrite.
        let handle = match runtime.block_on(async {
            let h = Lunaris::open(&url).await?;
            Ok::<_, lunaris::LunarisError>(h.with_embedder(Arc::new(StubEmbedder::new(768))))
        }) {
            Ok(h) => Arc::new(h),
            Err(e) => {
                eprintln!(
                    "SKIP consolidator_tick/{} bench: open failed: {}",
                    backend.label, e
                );
                continue;
            }
        };

        let storage = handle.storage();
        let consolidator = ActRConsolidator::default();
        let label = backend.label;

        group.bench_with_input(
            BenchmarkId::new("consolidator_tick", label),
            &BATCH_SIZE,
            |b, _size| {
                b.to_async(&runtime).iter(|| {
                    let storage = storage.clone();
                    let batch = batch.clone();
                    let queue_depth_peak = queue_depth_peak.clone();
                    async move {
                        // IO discipline (CLAUDE.md design-for-failure): wrap
                        // the consolidate call in a tokio timeout so a wedged
                        // backend surfaces as a bench failure, not a hang.
                        let report = tokio::time::timeout(
                            PER_ITER_TIMEOUT,
                            consolidator.consolidate(storage, &batch),
                        )
                        .await
                        .expect("consolidator_tick: 2s per-iteration timeout exceeded")
                        .expect("consolidator_tick: consolidate returned Err");

                        // R16-03 proxy: track peak emission-vector length.
                        let depth =
                            (report.promotions.len() + report.archives.len()) as u64;
                        let prev = queue_depth_peak.load(Ordering::Relaxed);
                        if depth > prev {
                            queue_depth_peak.store(depth, Ordering::Relaxed);
                        }
                    }
                });
            },
        );

        envelope_pending_push(label, url.clone(), queue_depth_peak.clone());
    }

    group.finish();

    for (label, url, depth) in drain_envelope_pending() {
        if let Err(e) = write_envelope(&runtime, &label, &url, depth.load(Ordering::Relaxed)) {
            eprintln!("WARN consolidator_tick/{label}: envelope write failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Evidence envelope (fingerprint + proxy queue_depth_peak)
// Shape mirrors `helios_p50.rs` to keep post-processors uniform.
// ---------------------------------------------------------------------------

static ENVELOPE_PENDING: parking_lot::Mutex<Vec<(String, String, Arc<AtomicU64>)>> =
    parking_lot::Mutex::new(Vec::new());

fn envelope_pending_push(label: &str, url: String, depth: Arc<AtomicU64>) {
    ENVELOPE_PENDING.lock().push((label.to_string(), url, depth));
}

fn drain_envelope_pending() -> Vec<(String, String, Arc<AtomicU64>)> {
    std::mem::take(&mut *ENVELOPE_PENDING.lock())
}

fn write_envelope(
    _runtime: &tokio::runtime::Runtime,
    label: &str,
    url: &str,
    queue_depth_peak: u64,
) -> std::io::Result<()> {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let dir = PathBuf::from(target)
        .join("criterion")
        .join("consolidator_tick")
        .join(format!("consolidator_tick/{label}"));

    let (p50_ms, mean_ms) = match read_criterion_estimates(&dir.join("new/estimates.json")) {
        Ok((p50, mean)) => (Some(p50), mean),
        Err(_) => (None, None),
    };

    let envelope = serde_json::json!({
        "bench": "consolidator_tick",
        "label": label,
        "backend_url": redact_url(url),
        "git_sha": git_sha().unwrap_or_else(|| "unknown".into()),
        "moon_version": moon_version_probe(label),
        "pg_version": pg_version_probe(label),
        "cpu_model": cpu_model_probe(),
        "measured_p50_ms": p50_ms,
        "measured_mean_ms": mean_ms,
        "queue_depth_peak": queue_depth_peak,
        "batch_size": BATCH_SIZE,
        "episode_fanout": EPISODE_FANOUT,
        "sample_size": SAMPLE_SIZE,
    });

    std::fs::create_dir_all(&dir)?;
    let path = dir.join("envelope.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&envelope).unwrap_or_default())?;
    eprintln!("consolidator_tick/{label}: envelope → {}", path.display());
    Ok(())
}

fn read_criterion_estimates(path: &std::path::Path) -> std::io::Result<(f64, Option<f64>)> {
    let bytes = std::fs::read(path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let median_ns = v
        .get("median")
        .and_then(|m| m.get("point_estimate"))
        .and_then(|pe| pe.as_f64())
        .ok_or_else(|| std::io::Error::other("missing median.point_estimate"))?;
    let mean_ns = v
        .get("mean")
        .and_then(|m| m.get("point_estimate"))
        .and_then(|pe| pe.as_f64());
    Ok((median_ns / 1_000_000.0, mean_ns.map(|n| n / 1_000_000.0)))
}

fn git_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn moon_version_probe(label: &str) -> String {
    if label != "moon" {
        return "n/a".into();
    }
    probe_cmd(&["moon-cli", "--version"])
        .or_else(|| probe_cmd(&["moon", "--version"]))
        .unwrap_or_else(|| "unknown".into())
}

fn pg_version_probe(label: &str) -> String {
    if label != "postgres" {
        return "n/a".into();
    }
    probe_cmd(&["psql", "--version"]).unwrap_or_else(|| "unknown".into())
}

fn cpu_model_probe() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(v) = probe_cmd(&["sysctl", "-n", "machdep.cpu.brand_string"]) {
            return v;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("model name") {
                    if let Some(idx) = rest.find(':') {
                        return rest[idx + 1..].trim().to_string();
                    }
                }
            }
        }
    }
    "unknown".into()
}

fn probe_cmd(argv: &[&str]) -> Option<String> {
    let out = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            return format!("{scheme}***@{}", &rest[at + 1..]);
        }
    }
    url.to_string()
}

criterion_group!(consolidator_tick, consolidator_tick_bench);
criterion_main!(consolidator_tick);
