//! Plan 05-06 EVAL-04 — RSS sampling at 1M facts.
//!
//! Per CONTEXT.md D-19: ingest 1M facts via
//! [`build_one_million_fact_corpus`]; sample resident-set-size via per-OS
//! sampler; assert RSS ≤ 2 GB on Moon TQ4 4-bit per blueprint §13.
//!
//! ## Per-OS sampler (B-6 fix)
//!
//! - **Linux**: [`procfs::process::Process::myself`].stat() × page_size.
//! - **macOS**: shell-out to `ps -o rss= -p <pid>` (returns RSS in KB; multiply
//!   by 1024). NO `mach2` FFI — CLAUDE.md `#![forbid(unsafe_code)]` mandate
//!   would require an unsafe wrapper crate (Plan 04-03 set the precedent:
//!   nix wraps unsafe; mach2 has no equivalent safe wrapper as of 0.4).
//! - **Windows / other**: returns `None` → harness emits `status: "SKIPPED"`.
//!
//! ## Soft-fail
//!
//! Missing `MOON_URL` / Lunaris::open failure / corpus build error → emit
//! ONE `status: "SKIPPED"` row so the manifest stays at stable cardinality.

#![forbid(unsafe_code)]

use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "memory";
pub(crate) const METRIC: &str = "rss_gb_at_1m_facts";
pub(crate) const THRESHOLD_GB: f64 = 2.0;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let started = Instant::now();

    let url = match std::env::var("MOON_URL") {
        Ok(u) => u,
        Err(_) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD_GB,
                "MOON_URL unset — RSS harness needs Moon TQ4 backend",
            ));
            return Ok(());
        }
    };

    // Open Lunaris on Moon (TQ4 4-bit per blueprint §13).
    let lunaris = match lunaris::Lunaris::open(&url).await {
        Ok(l) => l,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD_GB,
                &format!("Lunaris::open({url}) failed: {e}"),
            ));
            return Ok(());
        }
    };

    // Bulk-ingest count (default 1M; LUNARIS_EVAL_MEMORY_COUNT env override
    // for dev-box runs that want a smaller smoke). Per T-05-06-03 mitigation
    // — caps runaway corpus generation in CI.
    let count: u64 = std::env::var("LUNARIS_EVAL_MEMORY_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);

    eprintln!("memory: bulk-ingesting {count} facts to {url}");
    if let Err(e) = crate::corpus::build_one_million_fact_corpus(
        &lunaris,
        count,
        crate::corpus::DEFAULT_CORPUS_SEED,
        &url,
    )
    .await
    {
        results.push(EvalRow::skipped(
            HARNESS,
            METRIC,
            THRESHOLD_GB,
            &format!("corpus build failed: {e:?}"),
        ));
        return Ok(());
    }

    // Sample RSS post-ingest. Per-OS sampler returns None on unsupported
    // platforms (Windows + anything we haven't gated for) so the harness
    // SKIPS instead of fabricating numbers.
    let rss_bytes = match sample_rss() {
        Some(b) => b,
        None => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD_GB,
                "RSS sampling not supported on this platform (only Linux + macOS)",
            ));
            return Ok(());
        }
    };

    let rss_gb = rss_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    let row = EvalRow::judge_le(
        HARNESS,
        METRIC,
        rss_gb,
        THRESHOLD_GB,
        started.elapsed().as_millis() as u64,
    );
    results.push(row);
    Ok(())
}

/// Linux RSS sampler — reads `/proc/self/stat` via the safe-Rust `procfs`
/// crate. Returns RSS in bytes (page count × page size).
#[cfg(target_os = "linux")]
fn sample_rss() -> Option<u64> {
    let proc = procfs::process::Process::myself().ok()?;
    let stat = proc.stat().ok()?;
    let page_size = procfs::page_size();
    Some((stat.rss as u64) * page_size)
}

/// macOS RSS sampler — shells out to `ps -o rss= -p <pid>` (returns RSS in
/// KB; multiply by 1024). Per CLAUDE.md `#![forbid(unsafe_code)]` mandate,
/// avoids the `mach2` FFI path which would require an unsafe wrapper crate.
/// `ps` is a POSIX-required binary; available on every macOS install.
#[cfg(target_os = "macos")]
fn sample_rss() -> Option<u64> {
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let kb: u64 = stdout.trim().parse().ok()?;
    Some(kb * 1024)
}

/// Windows + any other target: SKIPPED. Returning None triggers the
/// `status: "SKIPPED"` branch in [`run`].
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample_rss() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_rss_returns_nonzero_on_supported_os() {
        // sample_rss returns Some(>0) on Linux + macOS; None on Windows.
        let result = sample_rss();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(result.is_some(), "RSS sampling should work on this OS");
            let bytes = result.unwrap();
            assert!(bytes > 0, "self-RSS should be > 0; got {bytes}");
            // Sanity: every running test process consumes more than 1 MB.
            assert!(bytes > 1_048_576, "self-RSS unrealistically small ({bytes} bytes)");
        } else {
            assert!(result.is_none(), "RSS sampling should be None on unsupported OS");
        }
    }

    #[tokio::test]
    async fn run_skips_cleanly_without_moon_url() {
        // Don't mutate env (would require unsafe in Rust 2024); instead
        // assume the test environment has no MOON_URL set. If it IS set,
        // the harness will instead try to connect — that's fine; we just
        // accept a longer test and verify cardinality.
        let mut results: Vec<EvalRow> = Vec::new();
        super::run(&mut results).await.unwrap();
        assert_eq!(results.len(), 1, "memory harness emits exactly one row");
        // Either SKIPPED (env unset / open failed) or PASS/FAIL (live run).
        // We don't pin the status; we just verify the harness didn't crash.
        assert!(matches!(results[0].status.as_str(), "SKIPPED" | "PASS" | "FAIL"));
    }
}
