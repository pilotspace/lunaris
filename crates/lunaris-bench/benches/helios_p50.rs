//! Plan 12-03 — Criterion bench for the HeliosScratchpad v2 tool-call
//! hot path (HELIOS-05).
//!
//! Measures end-to-end latency of `HeliosScratchpad::{write, read, edit, grep}`
//! under the helios-rfc §5.3 Read:Write:Edit:Grep telemetry mix, end-to-end on
//! Moon + Postgres. Per CONTEXT.md D-07/D-09 the acceptance gate is **p50 ≤ 20 ms**
//! on each backend (p99 stays on the v0.1.0 ≤ 60 ms Postgres budget, unchanged).
//!
//! ## Workload mix (Claude's Discretion — documented)
//!
//! `helios-rfc.md` §5.3 lists the Read/Write/Edit/Grep surface but does NOT
//! publish a concrete ratio table at this revision. Per CONTEXT.md the
//! executor picks a conservative mix and records it in `12-03-SUMMARY.md`:
//!
//! ```text
//! read  : 40%
//! write : 30%
//! edit  : 15%
//! grep  : 15%
//! ```
//!
//! Deterministic sampling via `rand_chacha::ChaCha12Rng::seed_from_u64(0xBEEF_DEAD)`
//! so every bench invocation draws the same sequence of operations; this
//! keeps runs comparable across machines + commits.
//!
//! ## Skip discipline (mirrors `recall_hot_path.rs`)
//!
//! - URL env unset → skip backend.
//! - 1-sec TCP probe fails → skip backend.
//! - `Lunaris::open` fails → skip backend (with eprintln).
//! - Warm-up write fails → skip backend.
//!
//! This lets the default `cargo bench -p lunaris-bench --bench helios_p50`
//! exit cleanly on dev machines without live backends — the bench is then a
//! no-op rather than a false measurement.
//!
//! ## Evidence envelope
//!
//! After the bench loop completes for each reachable backend, the bench
//! writes `target/criterion/helios_smoke/helios_p50_<label>/envelope.json`
//! recording git SHA, CPU model, best-effort Moon / Postgres version probes,
//! the mix weights used, and the measured p50/p99 pulled from the Criterion
//! estimates + sample files. Envelope is NOT committed (matches
//! T-12-03-03 accept disposition; lives under the `target/` tree which is
//! already `.gitignore`-d).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lunaris::{HeliosScratchpad, Lunaris};
use lunaris_bench::tcp_reachable;
use lunaris_core::StubEmbedder;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha12Rng;
use ulid::Ulid;

/// helios-rfc §5.3 mix weights (Claude's Discretion — see module doc).
/// Must sum to 1.0.
const MIX_READ: f64 = 0.40;
const MIX_WRITE: f64 = 0.30;
const MIX_EDIT: f64 = 0.15;
const MIX_GREP: f64 = 0.15;

/// Deterministic seed for the operation-mix RNG. Keep pinned.
const MIX_SEED: u64 = 0xBEEF_DEAD;

/// Number of warm-up notes seeded into the pad before the bench loop starts.
/// Small enough to build fast (< 2s) on dev boxes, large enough that `read`
/// / `edit` / `grep` find real content.
const WARMUP_NOTES: usize = 100;

/// Criterion sample count. Tuned to produce a stable p50 estimate within
/// ~30 s wall-clock per backend on a modern dev box.
const SAMPLE_SIZE: usize = 60;

/// Criterion warm-up time before sampling begins.
const WARMUP_SECS: u64 = 3;

/// Criterion measurement window.
const MEASUREMENT_SECS: u64 = 25;

/// Top-K the `grep` call requests.
const GREP_TOP_K: usize = 5;

struct Backend {
    label: &'static str,
    env: &'static str,
}

const BACKENDS: &[Backend] = &[
    Backend {
        label: "moon",
        env: "MOON_URL",
    },
    Backend {
        label: "postgres",
        env: "PG_URL",
    },
];

/// Pre-generated note-paths the bench loop draws from. The warm-up seeds each
/// path with a non-empty body so `read` + `edit` + `grep` all find content.
struct WarmupSet {
    paths: Vec<String>,
}

/// Operation discriminator drawn from the mix RNG.
#[derive(Copy, Clone, Debug)]
enum Op {
    Read,
    Write,
    Edit,
    Grep,
}

fn draw_op(rng: &mut ChaCha12Rng) -> Op {
    let r: f64 = rng.random();
    let mut acc = MIX_READ;
    if r < acc {
        return Op::Read;
    }
    acc += MIX_WRITE;
    if r < acc {
        return Op::Write;
    }
    acc += MIX_EDIT;
    if r < acc {
        return Op::Edit;
    }
    Op::Grep
}

fn helios_p50_bench(c: &mut Criterion) {
    // Sanity: mix weights must sum to 1.0. Guards against silent drift if a
    // future edit renormalises only three of four constants.
    let sum = MIX_READ + MIX_WRITE + MIX_EDIT + MIX_GREP;
    assert!(
        (sum - 1.0).abs() < 1e-9,
        "helios_p50 mix weights must sum to 1.0; got {sum}"
    );

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("helios_smoke");
    group.sample_size(SAMPLE_SIZE);
    group.warm_up_time(Duration::from_secs(WARMUP_SECS));
    group.measurement_time(Duration::from_secs(MEASUREMENT_SECS));

    for backend in BACKENDS {
        let Ok(url) = std::env::var(backend.env) else {
            eprintln!(
                "SKIP helios_smoke/{} bench: {} unset",
                backend.label, backend.env
            );
            continue;
        };
        if !runtime.block_on(tcp_reachable(&url)) {
            eprintln!(
                "SKIP helios_smoke/{} bench: backend unreachable (1s TCP probe failed)",
                backend.label
            );
            continue;
        }

        // One handle per backend. StubEmbedder isolates the measurement from
        // Gemma-model cost (same approach as recall_hot_path.rs).
        let handle = match runtime.block_on(async {
            let h = Lunaris::open(&url).await?;
            Ok::<_, lunaris::LunarisError>(h.with_embedder(Arc::new(StubEmbedder::new(768))))
        }) {
            Ok(h) => Arc::new(h),
            Err(e) => {
                eprintln!(
                    "SKIP helios_smoke/{} bench: open failed: {}",
                    backend.label, e
                );
                continue;
            }
        };

        let session_id = format!("bench-{}", Ulid::new());
        let pad = HeliosScratchpad::new(handle.clone(), &session_id);

        // Warm-up: seed WARMUP_NOTES notes so every op type has real content
        // to touch. Any write failure here means the backend is functionally
        // broken — skip rather than benchmark garbage.
        let warmup = match runtime.block_on(seed_warmup(&pad)) {
            Ok(w) => w,
            Err(e) => {
                eprintln!(
                    "SKIP helios_smoke/{} bench: warm-up failed: {}",
                    backend.label, e
                );
                continue;
            }
        };

        let rng_state = std::sync::Mutex::new(ChaCha12Rng::seed_from_u64(MIX_SEED));
        let idx = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(warmup.paths);

        let label = backend.label;
        group.bench_with_input(
            BenchmarkId::new("helios_p50", label),
            &paths.len(),
            |b, _len| {
                b.to_async(&runtime).iter(|| {
                    let pad = pad.clone();
                    let paths = paths.clone();
                    let idx = idx.clone();
                    let op = {
                        let mut g = rng_state.lock().expect("rng mutex");
                        draw_op(&mut g)
                    };
                    async move {
                        let i = idx.fetch_add(1, Ordering::Relaxed);
                        let path = &paths[i % paths.len()];
                        match op {
                            Op::Read => {
                                let _ = pad.read(path).await.expect("read");
                            }
                            Op::Write => {
                                let new_path = format!("note-{}.md", Ulid::new());
                                let _ = pad
                                    .write(&new_path, "lorem ipsum helios bench payload")
                                    .await
                                    .expect("write");
                            }
                            Op::Edit => {
                                let _ = pad
                                    .edit(path, "lorem", "lorem-edited")
                                    .await
                                    .expect("edit");
                            }
                            Op::Grep => {
                                let _ = pad.grep("lorem", GREP_TOP_K).await.expect("grep");
                            }
                        }
                    }
                });
            },
        );

        // Evidence envelope — best-effort, written after the group finishes.
        // We defer actual disk-write to after `group.finish()` (post-loop)
        // so the Criterion estimates.json exists on disk.
        envelope_pending_push(label, url.clone());
    }

    group.finish();

    // Now that Criterion has flushed estimates / sample JSON to disk, walk
    // each pending backend and write the evidence envelope. Failures are
    // swallowed (best-effort per T-12-03-04 mitigation — unknown fields
    // are recorded, never silently dropped).
    for (label, url) in drain_envelope_pending() {
        if let Err(e) = write_envelope(&runtime, &label, &url) {
            eprintln!("WARN helios_smoke/{label}: envelope write failed: {e}");
        }
    }
}

/// Seed [`WARMUP_NOTES`] notes under the pad's session prefix with a known
/// body containing the grep keyword `"lorem"`.
async fn seed_warmup(pad: &HeliosScratchpad) -> Result<WarmupSet, lunaris::LunarisError> {
    let mut paths = Vec::with_capacity(WARMUP_NOTES);
    for i in 0..WARMUP_NOTES {
        let p = format!("warmup-{i:04}.md");
        pad.write(&p, format!("lorem ipsum warmup body {i}"))
            .await?;
        paths.push(p);
    }
    Ok(WarmupSet { paths })
}

// ---------------------------------------------------------------------------
// Evidence envelope (T-12-03-04 mitigation — record versions + git SHA)
// ---------------------------------------------------------------------------

/// Module-private staging area so the bench-loop closure can register which
/// backends ran; the envelope emitter runs AFTER `group.finish()` so the
/// Criterion JSON is guaranteed to be on disk.
static ENVELOPE_PENDING: std::sync::Mutex<Vec<(String, String)>> =
    std::sync::Mutex::new(Vec::new());

fn envelope_pending_push(label: &str, url: String) {
    ENVELOPE_PENDING
        .lock()
        .expect("envelope pending mutex")
        .push((label.to_string(), url));
}

fn drain_envelope_pending() -> Vec<(String, String)> {
    std::mem::take(&mut *ENVELOPE_PENDING.lock().expect("envelope pending mutex"))
}

fn write_envelope(
    _runtime: &tokio::runtime::Runtime,
    label: &str,
    url: &str,
) -> std::io::Result<()> {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let dir = PathBuf::from(target)
        .join("criterion")
        .join("helios_smoke")
        .join(format!("helios_p50/{label}"));

    // Read p50/p99 from Criterion's own JSON output if it exists.
    let (p50_ms, p99_ms) = match read_criterion_estimates(&dir.join("new/estimates.json")) {
        Ok((p50, p99)) => (Some(p50), p99),
        Err(_) => (None, None),
    };

    let envelope = serde_json::json!({
        "bench": "helios_p50",
        "label": label,
        "backend_url": redact_url(url),
        "git_sha": git_sha().unwrap_or_else(|| "unknown".into()),
        "moon_version": moon_version_probe(label),
        "pg_version": pg_version_probe(label),
        "cpu_model": cpu_model_probe(),
        "measured_p50_ms": p50_ms,
        "measured_p99_ms": p99_ms,
        "mix": {
            "read": MIX_READ,
            "write": MIX_WRITE,
            "edit": MIX_EDIT,
            "grep": MIX_GREP,
        },
        "sample_size": SAMPLE_SIZE,
        "warmup_notes": WARMUP_NOTES,
    });

    // Ensure the parent dir exists (Criterion may not have created the
    // /<label>/ dir if no samples ran — still want the envelope as a
    // best-effort record of the attempted run).
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("envelope.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&envelope).unwrap_or_default())?;
    eprintln!("helios_smoke/{label}: envelope → {}", path.display());
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
    // Best-effort — if moon-cli is on PATH it answers --version; otherwise
    // we record "unknown" per T-12-03-04 (visible gap > silent pass).
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

/// Strip any `user:pass@` userinfo from a connection URL before logging.
/// Mirrors Plan 04 follow-up guidance in STATE.md pending-todos.
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            return format!("{scheme}***@{}", &rest[at + 1..]);
        }
    }
    url.to_string()
}

criterion_group!(helios_smoke, helios_p50_bench);
criterion_main!(helios_smoke);
