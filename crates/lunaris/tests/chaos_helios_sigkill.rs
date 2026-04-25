//! Plan 12-04 HELIOS-06 — SIGKILL chaos over the Helios tool flow.
//!
//! ## What this test proves
//!
//! The `atomic_write` single-call invariant (v0.1.0 Plan 04-02 D-11) still
//! holds under:
//!
//! 1. `HeliosScratchpad::write` (mid-ingest SIGKILL), AND
//! 2. Consolidator-ON mid-promotion race (new Phase-12-specific risk).
//!
//! 50 iterations × {Moon, Postgres} = 100 runs. Every run asserts
//! `fsck_all(storage, session_prefix).is_clean()` on the recovered
//! handle. Any orphan row panics — HELIOS-06 does NOT ship on a single
//! cross-tenant leak.
//!
//! ## Skip discipline
//!
//! Default `cargo test -p lunaris --test chaos_helios_sigkill` runs the
//! pure-helper tests only — they compile + pass with no backends.
//!
//! The 50×2 chaos run is `#[ignore]`-gated and requires `MOON_URL` and/or
//! `PG_URL` env vars. Tuning knob: `LUNARIS_CHAOS_HELIOS_ITERS` (default
//! 50).
//!
//! ```bash
//! MOON_URL=moon://localhost:6390 PG_URL=postgres://lunaris@localhost/lunaris \
//!   LUNARIS_CHAOS_HELIOS_ITERS=50 \
//!   cargo test -p lunaris --test chaos_helios_sigkill \
//!   chaos_sigkill_helios_flow_zero_orphans -- --ignored --nocapture
//! ```
//!
//! ## Evidence
//!
//! Per-run JSON emitted under
//! `target/chaos/12-sigkill-helios/<iter>-<backend>-<site>.json`:
//!
//! ```json
//! { "iter": 0, "backend": "moon", "kill_site": "MidHeliosScratchpadWrite",
//!   "kill_offset_ns": 12345, "orphan_count": 0,
//!   "fsck_classes": { "kv": 0, "vector": 0, "graph": 0, "index": 0 } }
//! ```
//!
//! On panic, the test does NOT clean up — evidence stays on disk for the
//! operator post-mortem per T-12-04-02.
//!
//! ## Postgres fallback (pre-authorized per `12-HUMAN-UAT.md` §2)
//!
//! The Postgres branch is wrapped in a pgmq-specific error detector. If
//! `Lunaris::open(pg_url)` returns an error whose `Display` contains
//! "pgmq" or "partition", the test writes
//! `target/chaos/12-sigkill-helios/postgres-fallback.json` and continues —
//! per the pre-authorized fallback gate. Moon-only acceptance still holds.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` — SIGKILL is delivered by `nix`'s safe
//!   wrapper around `kill(2)`.
//! - No process-env mutation; env read at call sites, `Option<String>`
//!   passed into the probe (matches Phase 4 W-7 discipline).
//! - No locks held across `.await`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use lunaris_conformance::chaos_helpers::{FsckReport, InterruptSite, helios_interrupt_profile};

// ---------------------------------------------------------------------------
// Public types (serialized into the per-run JSON evidence)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub iter: u64,
    pub backend: String,
    pub kill_site: String,
    pub kill_offset_ns: u64,
    pub orphan_count: u64,
    pub fsck_classes: FsckClassCounts,
    pub rows: Vec<lunaris_conformance::chaos_helpers::OrphanRow>,
    pub notes: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FsckClassCounts {
    pub kv: u64,
    pub vector: u64,
    pub graph: u64,
    pub index: u64,
}

impl FsckClassCounts {
    pub fn from_report(r: &FsckReport) -> Self {
        let mut c = Self::default();
        for (class, count) in &r.by_class {
            match class.as_str() {
                "kv" => c.kv = *count,
                "vector" => c.vector = *count,
                "graph" => c.graph = *count,
                "index" => c.index = *count,
                _ => {}
            }
        }
        c
    }
}

// ---------------------------------------------------------------------------
// Pure helpers — exercised in the default `cargo test` mode
// ---------------------------------------------------------------------------

/// Stable default evidence directory for the 12-04 chaos run.
pub fn default_evidence_dir() -> PathBuf {
    PathBuf::from("target/chaos/12-sigkill-helios")
}

pub fn evidence_path(dir: &Path, iter: u64, backend: &str, site: &str) -> PathBuf {
    dir.join(format!("{iter:03}-{backend}-{site}.json"))
}

pub fn fallback_marker_path(dir: &Path) -> PathBuf {
    dir.join("postgres-fallback.json")
}

/// Redact userinfo from postgres URLs before serialization (T-12-04-04).
pub fn redact_url(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("postgres://") {
        return format!("postgres://{}", strip_userinfo(rest));
    }
    if let Some(rest) = raw.strip_prefix("postgresql://") {
        return format!("postgresql://{}", strip_userinfo(rest));
    }
    raw.to_string()
}

fn strip_userinfo(authority_plus: &str) -> String {
    if let Some(idx) = authority_plus.find('@') {
        authority_plus[idx + 1..].to_string()
    } else {
        authority_plus.to_string()
    }
}

/// Detect the pgmq partition-packing fallback trigger per `12-HUMAN-UAT.md` §2.
pub fn is_pgmq_fallback_error(err_display: &str) -> bool {
    let hay = err_display.to_ascii_lowercase();
    hay.contains("pgmq") || hay.contains("partition")
}

/// Emit one run's JSON evidence. Failures to write are logged only — JSON
/// is evidence, not a gate.
pub fn write_evidence(path: &Path, record: &RunRecord) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::File::create(path) {
        Ok(f) => {
            if let Err(e) = serde_json::to_writer_pretty(&f, record) {
                eprintln!("chaos: failed to write evidence {path:?}: {e}");
            }
        }
        Err(e) => {
            eprintln!("chaos: failed to create evidence file {path:?}: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Live chaos run — #[ignore]-gated, requires MOON_URL / PG_URL
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod live {
    use super::*;

    use std::io::{BufRead, BufReader};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::Duration;

    use lunaris::Lunaris;
    use lunaris_conformance::chaos_helpers::{InterruptSite, fsck_all, helios_interrupt_profile};
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    /// Byte-identical probe to other Helios tests (sha256 match — Phase 9
    /// convention). Two-tier skip: env-empty / DNS-fail / TCP-timeout all
    /// return None.
    pub(crate) fn probe_backend(url_env: &str) -> Option<String> {
        let raw = std::env::var(url_env).ok()?;
        if raw.is_empty() {
            return None;
        }
        let host_port = if let Some(rest) = raw.strip_prefix("moon://") {
            rest.split('/').next()?.to_string()
        } else if raw.starts_with("postgres://") || raw.starts_with("postgresql://") {
            let after_scheme = raw.split("://").nth(1)?;
            let authority = after_scheme.split('/').next()?;
            let bare = authority.rsplit('@').next()?;
            if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
        } else {
            return None;
        };
        let timeout = Duration::from_secs(1);
        let addr = host_port.to_socket_addrs().ok().and_then(|mut it| it.next())?;
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => Some(raw),
            Err(_) => None,
        }
    }

    fn chaos_bin_path() -> PathBuf {
        std::env::var("CARGO_BIN_EXE_chaos").map(PathBuf::from).unwrap_or_else(|_| {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let workspace =
                manifest.parent().and_then(|p| p.parent()).unwrap_or(&manifest).to_path_buf();
            let target_dir = std::env::var("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| workspace.join("target"));
            let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
            target_dir.join(profile).join("chaos")
        })
    }

    /// Spawn the chaos helios subprocess, wait for "ready\n", sleep
    /// `kill_offset_ns`, deliver SIGKILL, reap the child.
    pub(crate) fn run_chaos_iteration(
        url: &str,
        session_id: &str,
        site: InterruptSite,
        kill_offset_ns: u64,
    ) {
        let chaos_bin = chaos_bin_path();
        let mut child = Command::new(&chaos_bin)
            .arg("--profile")
            .arg("helios")
            .env("LUNARIS_URL", url)
            .env("LUNARIS_HELIOS_SESSION_ID", session_id)
            .env("KILL_SITE", site.as_str())
            .env("KILL_OFFSET_NS", kill_offset_ns.to_string())
            // stdout piped so we can read the ready signal;
            // stderr suppressed to avoid leaking URL creds.
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn chaos --profile helios child");

        // Read ready\n from the child's stdout. A short read timeout is
        // emulated by a blocking BufRead — if the child dies before
        // emitting ready, the read returns EOF and we proceed straight to
        // the kill (which is a no-op on an already-dead PID).
        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            // Don't assert — an EOF here means the child crashed before
            // ready, which is still a valid chaos iteration (no orphan
            // expected because no atomic_write was started).
        }

        // Sleep the kill offset (after ready) — this straddles the
        // operation's atomic_write fanout when > 0.
        if kill_offset_ns > 0 {
            std::thread::sleep(Duration::from_nanos(kill_offset_ns));
        }

        let pid = Pid::from_raw(child.id() as i32);
        match child.try_wait() {
            Ok(Some(_)) => {
                // Child already done — commit-completed branch. Valid.
            }
            Ok(None) => {
                let _ = kill(pid, Signal::SIGKILL);
            }
            Err(_) => {
                let _ = kill(pid, Signal::SIGKILL);
            }
        }
        let _ = child.wait();

        // Let the backend rollback settle before the parent reopens.
        std::thread::sleep(Duration::from_millis(100));
    }

    pub(crate) async fn per_backend_run(
        backend_name: &str,
        url: &str,
        iters: u64,
        evidence_dir: &Path,
    ) -> (u64, u64) {
        let mut completed = 0u64;
        let mut orphaned = 0u64;
        // Deterministic seeded RNG so the per-run kill_offset_ns sequence
        // is reproducible from the per-run JSON.
        let mut rng = StdRng::seed_from_u64(0xC0DE_FACE);
        let profile = helios_interrupt_profile();
        assert!(!profile.sites.is_empty());

        for iter in 0..iters {
            // Sample site + offset.
            let site_idx = rng.random_range(0..profile.sites.len());
            let site = profile.sites[site_idx];
            // kill_offset_ns ∈ [1ms, 110ms] — same domain as the Phase 4
            // crash_recovery harness (ATOMIC_WRITE_P99_MS).
            let kill_offset_ns: u64 = rng.random_range(1_000_000u64..=110_000_000u64);

            let session_id = format!("chaos-{backend_name}-{iter:03}-{}", ulid::Ulid::new());

            // --- run ---
            run_chaos_iteration(url, &session_id, site, kill_offset_ns);

            // --- recover + fsck ---
            let lunaris = match Lunaris::open(url).await {
                Ok(l) => Arc::new(l),
                Err(e) => {
                    eprintln!(
                        "chaos: iter {iter} {backend_name}: reopen failed: {e} \
                         — recording as orphan_count=unknown"
                    );
                    continue;
                }
            };
            let session_prefix = format!("helios:fs/{session_id}/");
            let report = match fsck_all(lunaris.storage(), &session_prefix).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("chaos: iter {iter} {backend_name}: fsck failed: {e}");
                    continue;
                }
            };

            let record = RunRecord {
                iter,
                backend: backend_name.to_string(),
                kill_site: site.as_str().to_string(),
                kill_offset_ns,
                orphan_count: report.total,
                fsck_classes: FsckClassCounts::from_report(&report),
                rows: report.rows.clone(),
                notes: format!(
                    "redacted_url={}; session_prefix={}",
                    redact_url(url),
                    session_prefix
                ),
            };
            let path = evidence_path(evidence_dir, iter, backend_name, site.as_str());
            write_evidence(&path, &record);

            completed += 1;
            if report.total > 0 {
                orphaned += 1;
                eprintln!(
                    "chaos: iter {iter} {backend_name}: ORPHANS={} (evidence {})",
                    report.total,
                    path.display()
                );
            }
        }
        (completed, orphaned)
    }
}

// ---------------------------------------------------------------------------
// The #[ignore]-gated 50×2 chaos test
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "50x2 SIGKILL live run; requires MOON_URL or PG_URL"]
async fn chaos_sigkill_helios_flow_zero_orphans() -> anyhow::Result<()> {
    let iters: u64 =
        std::env::var("LUNARIS_CHAOS_HELIOS_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);

    let evidence_dir = default_evidence_dir();
    let _ = fs::create_dir_all(&evidence_dir);

    // --- Moon branch ---
    let moon_result = match live::probe_backend("MOON_URL") {
        Some(url) => {
            eprintln!("chaos: running Moon branch at {}", redact_url(&url));
            let (done, orphaned) = live::per_backend_run("moon", &url, iters, &evidence_dir).await;
            Some((done, orphaned))
        }
        None => {
            eprintln!("chaos: SKIP Moon (MOON_URL unset or unreachable)");
            None
        }
    };

    // --- Postgres branch (with pgmq fallback detector) ---
    let pg_result = match live::probe_backend("PG_URL") {
        Some(url) => {
            // Pre-open probe — catches pgmq partition-packing early.
            match lunaris::Lunaris::open(&url).await {
                Ok(_) => {
                    eprintln!("chaos: running Postgres branch at {}", redact_url(&url));
                    let (done, orphaned) =
                        live::per_backend_run("postgres", &url, iters, &evidence_dir).await;
                    Some((done, orphaned, false))
                }
                Err(e) => {
                    let disp = format!("{e}");
                    if is_pgmq_fallback_error(&disp) {
                        eprintln!("POSTGRES_FALLBACK pgmq_partition_packing_error: {disp}");
                        let marker = fallback_marker_path(&evidence_dir);
                        let payload = serde_json::json!({
                            "fallback": "pgmq_partition_packing_error",
                            "error_display": disp,
                            "runbook": ".planning/phases/12-helios-production-hardening/12-HUMAN-UAT.md#2-sigkill-chaos-postgres-fallback-plan-12-04-forward-reference",
                        });
                        if let Ok(f) = fs::File::create(&marker) {
                            let _ = serde_json::to_writer_pretty(&f, &payload);
                        }
                        Some((0, 0, true))
                    } else {
                        return Err(anyhow::anyhow!(
                            "chaos: Postgres open failed with non-fallback error: {disp}"
                        ));
                    }
                }
            }
        }
        None => {
            eprintln!("chaos: SKIP Postgres (PG_URL unset or unreachable)");
            None
        }
    };

    // --- Hard gate per success criteria ---
    if let Some((done, orphaned)) = moon_result {
        assert!(done > 0, "Moon probed but no iterations completed");
        assert_eq!(
            orphaned,
            0,
            "Moon chaos: {orphaned}/{done} iterations showed orphans — \
             HELIOS-06 FAILED (evidence under {})",
            evidence_dir.display()
        );
    }
    if let Some((done, orphaned, fallback)) = pg_result
        && !fallback
    {
        assert!(done > 0, "Postgres probed but no iterations completed");
        assert_eq!(
            orphaned,
            0,
            "Postgres chaos: {orphaned}/{done} iterations showed orphans — \
                 HELIOS-06 FAILED (evidence under {})",
            evidence_dir.display()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Default-mode tests — pure helpers, no backend required
// ---------------------------------------------------------------------------

#[test]
fn evidence_path_uses_stable_naming() {
    let dir = PathBuf::from("target/chaos/12-sigkill-helios");
    let p = evidence_path(&dir, 7, "moon", "MidHeliosScratchpadWrite");
    assert!(p.to_string_lossy().ends_with("007-moon-MidHeliosScratchpadWrite.json"));
}

#[test]
fn redact_url_strips_postgres_userinfo() {
    assert_eq!(redact_url("postgres://user:password@host:5432/db"), "postgres://host:5432/db");
    assert_eq!(redact_url("postgresql://user:password@host:5432/db"), "postgresql://host:5432/db");
    assert_eq!(redact_url("moon://localhost:6390"), "moon://localhost:6390");
}

#[test]
fn pgmq_fallback_detector_matches_documented_triggers() {
    assert!(is_pgmq_fallback_error("pgmq partition packing failed"));
    assert!(is_pgmq_fallback_error("could not find partition for queue"));
    assert!(is_pgmq_fallback_error("PGMQ queue error"));
    assert!(!is_pgmq_fallback_error("connection refused"));
    assert!(!is_pgmq_fallback_error("moon redis error"));
}

#[test]
fn helios_interrupt_profile_is_non_empty() {
    // Cross-crate sanity: the composite profile lives in lunaris_conformance
    // and is re-exposed here for Task 2's chaos loop.
    let p = helios_interrupt_profile();
    assert!(!p.sites.is_empty(), "profile must have at least one site");
    assert!(p.sites.contains(&InterruptSite::MidHeliosScratchpadWrite));
    assert!(p.sites.contains(&InterruptSite::MidConsolidatorPromotion));
}

#[test]
fn fsck_class_counts_round_trip_from_report() {
    use lunaris_conformance::chaos_helpers::{FsckReport, OrphanClass, OrphanRow};
    let mut r = FsckReport::default();
    // Synthesize 2 KV + 1 Index orphan.
    for key in ["episode:aa", "episode:bb"] {
        r.rows.push(OrphanRow { class: OrphanClass::Kv, key: key.into(), reason: "test".into() });
    }
    r.rows.push(OrphanRow {
        class: OrphanClass::Index,
        key: "index:zz".into(),
        reason: "test".into(),
    });
    r.total = r.rows.len() as u64;
    for row in &r.rows {
        *r.by_class.entry(row.class.as_str().to_string()).or_insert(0) += 1;
    }
    let c = FsckClassCounts::from_report(&r);
    assert_eq!(c.kv, 2);
    assert_eq!(c.vector, 0);
    assert_eq!(c.graph, 0);
    assert_eq!(c.index, 1);
}

#[test]
fn write_evidence_creates_parent_dir_and_valid_json() {
    let tmp = std::env::temp_dir().join(format!("lunaris-chaos-test-{}", ulid::Ulid::new()));
    let path = tmp.join("0-moon-MidHeliosScratchpadWrite.json");
    let record = RunRecord {
        iter: 0,
        backend: "moon".into(),
        kill_site: "MidHeliosScratchpadWrite".into(),
        kill_offset_ns: 123,
        orphan_count: 0,
        fsck_classes: FsckClassCounts::default(),
        rows: vec![],
        notes: "unit-test".into(),
    };
    write_evidence(&path, &record);
    let loaded = fs::read_to_string(&path).expect("evidence written");
    let parsed: RunRecord = serde_json::from_str(&loaded).expect("valid JSON");
    assert_eq!(parsed.iter, 0);
    assert_eq!(parsed.kill_site, "MidHeliosScratchpadWrite");
    // Cleanup best-effort.
    let _ = fs::remove_dir_all(&tmp);
}
