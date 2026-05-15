//! O-03-A — per-device perf-contract regression gate.
//!
//! Walks a Criterion output directory (`target/criterion/` by default),
//! reads every `<group>/<bench>/change/estimates.json` that Criterion
//! produced when a bench run was invoked with `--baseline <NAME>`, and
//! exits non-zero if **any** bench's median timing regressed by more
//! than `--threshold` (default 5%).
//!
//! ## Why this binary exists
//!
//! Criterion prints `regressed` / `improved` markers to stdout but does
//! NOT set a non-zero exit code on regression. CI cannot block a merge
//! on stdout grepping alone (annotation parsing is brittle), so we walk
//! the on-disk JSON instead. `change/estimates.json` only exists when a
//! `--baseline` flag was passed AND a saved baseline exists; if it's
//! absent the bench either ran without comparison or the baseline was
//! never captured. Both cases are reported but DO NOT fail by default
//! (see `--require-comparison`).
//!
//! ## Criterion JSON schema (load-bearing)
//!
//! `target/criterion/<group>/<bench>/change/estimates.json`:
//!
//! ```json
//! {
//!   "mean":   { "point_estimate": 0.05, ... },
//!   "median": { "point_estimate": 0.03, ... }
//! }
//! ```
//!
//! `point_estimate` is the **relative change** (0.05 = +5%). The 5% cliff
//! gates on `median.point_estimate` — median is more robust than mean to
//! the cold-cache spike on the first few iterations.
//!
//! ## Two gate types — relative vs absolute
//!
//! This binary enforces the **relative** drift gate (no >5% regression vs
//! the saved baseline). The **absolute** hardware-target gates (Metal
//! embed p50 ≤ 5 ms, etc.) are a separate check that requires the bench
//! to have produced real numbers against staged weights — those are
//! documented in `docs/benchmarks/v0.4.x/README.md` and asserted by the
//! operator before the baseline is blessed in O3-C.
//!
//! ## Invocation
//!
//! ```sh
//! # In CI after `cargo bench -- --baseline v0.4-O01`:
//! cargo run -p lunaris-bench --bin perf-gate-check -- \
//!     --criterion-dir target/criterion \
//!     --threshold 0.05
//! # exit 0 on green, 1 on regression, 2 on operator error.
//! ```

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

/// Schema of `change/estimates.json` produced by Criterion when a bench
/// runs with `--baseline <name>`. Only the fields we read are modelled;
/// `serde` defaults to ignoring unknown fields.
#[derive(Debug, Deserialize)]
struct ChangeEstimates {
    median: PointEstimate,
    // `mean` is present in the file but we gate on median for stability.
    // Kept undeclared so the struct stays small + serde skips it.
}

#[derive(Debug, Deserialize)]
struct PointEstimate {
    /// Relative change vs baseline (0.05 = +5%, -0.02 = -2%).
    point_estimate: f64,
}

#[derive(Parser, Debug)]
#[command(
    name = "perf-gate-check",
    about = "O-03 — fail CI if any Criterion bench regressed > threshold vs the saved baseline."
)]
struct Args {
    /// Path to the Criterion output root.
    #[arg(long, default_value = "target/criterion")]
    criterion_dir: PathBuf,

    /// Maximum allowed relative regression on `median.point_estimate`.
    /// Default 0.05 = 5%, per HARDWARE-OPTIMIZATION-ROADMAP O-03.
    #[arg(long, default_value_t = 0.05)]
    threshold: f64,

    /// If set, treat "no `change/estimates.json` files found" as an error
    /// (operator forgot to pass `--baseline` to `cargo bench`). Default
    /// false so a `cargo bench --save-baseline` run on a fresh runner
    /// doesn't fail this gate.
    #[arg(long)]
    require_comparison: bool,
}

/// One regression hit — file path + observed median delta.
#[derive(Debug)]
struct Regression {
    path: PathBuf,
    median_delta: f64,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(e) => {
            eprintln!("perf-gate-check: operator error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns the count of regressions (0 = green). Operator errors (missing
/// dir, unreadable JSON) bubble up as `Err` and map to exit code 2 so CI
/// can tell "regression detected" (1) apart from "harness misconfigured" (2).
fn run(args: &Args) -> Result<usize> {
    if !args.criterion_dir.exists() {
        anyhow::bail!(
            "Criterion output directory {:?} not found — did `cargo bench` run?",
            args.criterion_dir
        );
    }

    let change_files = find_change_files(&args.criterion_dir)
        .with_context(|| format!("walking {:?}", args.criterion_dir))?;

    if change_files.is_empty() {
        if args.require_comparison {
            anyhow::bail!(
                "no change/estimates.json files under {:?} — did `cargo bench` run with `--baseline <name>` after a saved baseline existed?",
                args.criterion_dir
            );
        }
        eprintln!(
            "perf-gate-check: no comparison files under {:?} (run with --baseline to produce them); treating as PASS",
            args.criterion_dir
        );
        return Ok(0);
    }

    let mut regressions = Vec::new();
    let mut checked = 0usize;
    for f in &change_files {
        let delta = read_median_delta(f)
            .with_context(|| format!("reading {}", f.display()))?;
        checked += 1;
        let bench_label = pretty_label(f, &args.criterion_dir);
        if delta > args.threshold {
            println!("REGRESSION  {bench_label}  median {:+.2}%", delta * 100.0);
            regressions.push(Regression {
                path: f.clone(),
                median_delta: delta,
            });
        } else if delta > 0.0 {
            println!("ok          {bench_label}  median {:+.2}%", delta * 100.0);
        } else {
            println!("improved    {bench_label}  median {:+.2}%", delta * 100.0);
        }
    }

    println!();
    println!(
        "perf-gate-check: {checked} bench(es) checked vs baseline, threshold = {:.1}%, {} regression(s)",
        args.threshold * 100.0,
        regressions.len()
    );
    if !regressions.is_empty() {
        eprintln!();
        eprintln!("::error::perf-gate-check failed — {} bench(es) regressed > {:.1}%:", regressions.len(), args.threshold * 100.0);
        for r in &regressions {
            eprintln!("  - {}  median {:+.2}%", r.path.display(), r.median_delta * 100.0);
        }
    }
    Ok(regressions.len())
}

/// Find every `change/estimates.json` under `root`. Criterion's layout is
/// `target/criterion/<group>/<bench>/change/estimates.json`; we walk
/// generically (no fixed depth) to tolerate nested groups.
fn find_change_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            // Skip Criterion's per-bench `report/` HTML output — saves a few hundred
            // recursive stats on a typical run.
            if p.file_name().and_then(|n| n.to_str()) == Some("report") {
                continue;
            }
            walk(&p, out)?;
        } else if p.file_name().and_then(|n| n.to_str()) == Some("estimates.json")
            && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("change")
        {
            out.push(p);
        }
    }
    Ok(())
}

fn read_median_delta(path: &Path) -> Result<f64> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let parsed: ChangeEstimates = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed.median.point_estimate)
}

/// Produce `<group>/<bench>` for the human-readable log line.
fn pretty_label(file: &Path, root: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    // rel is `<group>/<bench>/change/estimates.json`; strip the tail.
    let mut comps: Vec<_> = rel.components().collect();
    if comps.len() >= 2 {
        comps.truncate(comps.len() - 2);
    }
    comps
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a fake Criterion tree at `root` with two benches; `bench_a`
    /// regressed by `a_delta`, `bench_b` by `b_delta`.
    fn make_tree(root: &Path, a_delta: f64, b_delta: f64) {
        for (bench, delta) in [("bench_a", a_delta), ("bench_b", b_delta)] {
            let dir = root.join("group_one").join(bench).join("change");
            fs::create_dir_all(&dir).unwrap();
            let body = format!(
                r#"{{
                    "mean":   {{ "point_estimate": {delta}, "standard_error": 0.0, "confidence_interval": {{"confidence_level": 0.95, "lower_bound": 0.0, "upper_bound": 0.0}} }},
                    "median": {{ "point_estimate": {delta}, "standard_error": 0.0, "confidence_interval": {{"confidence_level": 0.95, "lower_bound": 0.0, "upper_bound": 0.0}} }}
                }}"#
            );
            fs::write(dir.join("estimates.json"), body).unwrap();
        }
    }

    #[test]
    fn green_when_all_under_threshold() {
        let tmp = tempdir();
        make_tree(&tmp, 0.03, -0.01); // +3% and -1% — both pass the 5% gate.
        let args = Args {
            criterion_dir: tmp.clone(),
            threshold: 0.05,
            require_comparison: true,
        };
        let n = run(&args).expect("ok");
        assert_eq!(n, 0, "expected zero regressions");
    }

    #[test]
    fn red_on_single_regression() {
        let tmp = tempdir();
        make_tree(&tmp, 0.03, 0.07); // +3% (pass) and +7% (FAIL the 5% gate).
        let args = Args {
            criterion_dir: tmp.clone(),
            threshold: 0.05,
            require_comparison: true,
        };
        let n = run(&args).expect("ok");
        assert_eq!(n, 1, "expected exactly one regression (bench_b)");
    }

    #[test]
    fn red_on_multiple_regressions() {
        let tmp = tempdir();
        make_tree(&tmp, 0.10, 0.20);
        let args = Args {
            criterion_dir: tmp.clone(),
            threshold: 0.05,
            require_comparison: true,
        };
        let n = run(&args).expect("ok");
        assert_eq!(n, 2);
    }

    #[test]
    fn missing_dir_is_operator_error() {
        let bogus = PathBuf::from("/nonexistent/criterion-output-xyz");
        let args = Args {
            criterion_dir: bogus,
            threshold: 0.05,
            require_comparison: false,
        };
        assert!(run(&args).is_err(), "expected operator error on missing dir");
    }

    #[test]
    fn no_change_files_is_pass_by_default() {
        let tmp = tempdir();
        // Empty tree — no change/estimates.json anywhere.
        let args = Args {
            criterion_dir: tmp.clone(),
            threshold: 0.05,
            require_comparison: false,
        };
        let n = run(&args).expect("ok");
        assert_eq!(n, 0);
    }

    #[test]
    fn no_change_files_is_error_when_required() {
        let tmp = tempdir();
        let args = Args {
            criterion_dir: tmp,
            threshold: 0.05,
            require_comparison: true,
        };
        assert!(run(&args).is_err());
    }

    #[test]
    fn pretty_label_strips_tail() {
        let root = PathBuf::from("/c");
        let file = PathBuf::from("/c/embed/metal/batch_8/change/estimates.json");
        assert_eq!(pretty_label(&file, &root), "embed/metal/batch_8");
    }

    /// Minimal tempdir helper — avoids pulling tempfile as a new dep.
    /// Uses `std::env::temp_dir()` + a ULID-ish suffix; cleaned at test
    /// exit by relying on the OS's tmp sweeper (tests are idempotent).
    fn tempdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("lunaris-perf-gate-{nanos}-{:p}", &nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
