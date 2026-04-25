//! `cargo xtask` — Lunaris dev tasks.
//!
//! Plan 02-04 adds the `bench` subcommand that runs the full Phase 2
//! latency gauntlet against MoonStorage AND PostgresStorage, then prints
//! a Moon-vs-Postgres delta markdown table to stdout. Exit code is 0 when
//! Moon meets every budget; 1 when Moon misses (Phase 2 ship-blocker);
//! 0-with-warning when only Postgres misses (per ROADMAP risk register
//! soft-fail).
//!
//! ```text
//! Phase 2 Latency Report — Moon vs Postgres (1M-fact corpus, 50 samples)
//!
//! | Bench                            | Moon p50  | Moon p99  | PG p50    | PG p99    | Δ p50  | Δ p99  |
//! | -------------------------------- | --------- | --------- | --------- | --------- | ------ | ------ |
//! | ingest_hot_path/ingest_12kb_md   | 42.3 ms   | 95.8 ms   | 71.2 ms   | 153.4 ms  | +68%   | +60%   |
//! | recall_hot_path/recall_q         | 22.1 ms   | 67.4 ms   | 51.8 ms   | 142.0 ms  | +134%  | +110%  |
//! | atomic_write_hot_path/atomic_w   | 2.6 ms    | 9.1 ms    | 6.4 ms    | 18.7 ms   | +146%  | +106%  |
//!
//! ✓ Moon: all budgets met
//! ✗ Postgres: ingest p99 over budget (153.4 ms > 110 ms)
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "xtask", version, about = "Lunaris dev tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the Phase 2 latency gauntlet against both backends and print
    /// the Moon-vs-Postgres delta table.
    Bench {
        /// Skip the actual `cargo bench` invocation and only re-print the
        /// delta table from existing Criterion data on disk. Useful when
        /// the data is already there from a previous run.
        #[arg(long)]
        skip_bench: bool,
        /// Comma-separated list of bench groups to run. Defaults to all
        /// three.
        #[arg(long, default_value = "ingest_hot_path,recall_hot_path,atomic_write_hot_path")]
        groups: String,
    },
    /// Plan 05-06 — run the full eval gauntlet locally via `cargo run
    /// --release --bin lunaris-evals -- all --output <PATH>`. Writes a JSON
    /// manifest with one EvalRow per harness/metric per CONTEXT.md D-20.
    Eval {
        /// Output JSON manifest path (default `eval-results.json` at workspace root).
        #[arg(long, default_value = "eval-results.json")]
        output: PathBuf,
    },
    /// Plan 05-02 / 05-03 — run the conformance suite against a backend.
    /// Wraps `cargo test -p lunaris-conformance --test run_storage_<backend>`
    /// (and the protocol suite when the chosen backend is `protocol`).
    Conformance {
        /// Backend to certify: `moon` | `postgres`.
        #[arg(long, value_parser = ["moon", "postgres"])]
        backend: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            print_help();
            Ok(())
        }
        Some(Cmd::Bench { skip_bench, groups }) => cmd_bench(skip_bench, &groups),
        Some(Cmd::Eval { output }) => cmd_eval_gauntlet(&output),
        Some(Cmd::Conformance { backend }) => cmd_conformance(&backend),
    }
}

fn print_help() {
    println!("xtask — Lunaris dev tasks");
    println!();
    println!("usage: cargo xtask <subcommand>");
    println!();
    println!("subcommands:");
    println!("  bench                Run Phase 2 latency gauntlet (Moon + Postgres) + print");
    println!("                       Moon-vs-Postgres delta markdown table.");
    println!("                       Set MOON_URL / PG_URL to enable each backend; benches");
    println!("                       skip gracefully when env vars are unset.");
    println!("  bench --skip-bench   Re-print the delta table from existing Criterion data.");
    println!("  eval                 Run the Plan 05-06 eval gauntlet locally; writes");
    println!("                       eval-results.json (--output overrides path).");
    println!("                       Set MOON_URL / PG_URL / LUNARIS_EVAL_CACHE_DIR /");
    println!("                       LUNARIS_EXTRACT_GEMMA_PATH to populate the relevant");
    println!("                       harnesses (else they SKIP cleanly).");
    println!("  conformance --backend moon|postgres");
    println!("                       Run the lunaris-conformance suite against a backend.");
    println!("  help                 Print this help.");
}

// ----- eval subcommand (Plan 05-06) -----
//
// Wraps `cargo run --release --bin lunaris-evals -- all --output <PATH>`.
// The `eval` subcommand name refers to the Plan 05-06 EVAL-* gauntlet
// (LongMemEval, LoCoMo, ER-F1, memory, e2e, perf-delta) — NOT to dynamic
// code evaluation. Renamed the dispatch fn `cmd_eval_gauntlet` to make the
// distinction unambiguous for code reviewers + static analyzers.

fn cmd_eval_gauntlet(output: &Path) -> Result<()> {
    println!("xtask eval — invoking lunaris-evals all --output {}", output.display());
    let status = Command::new("cargo")
        .args(["run", "--release", "--bin", "lunaris-evals", "--", "all", "--output"])
        .arg(output)
        .status()
        .with_context(|| "invoke cargo run --bin lunaris-evals")?;
    if !status.success() {
        anyhow::bail!(
            "lunaris-evals exited with status {:?} — see {} for FAIL rows",
            status.code(),
            output.display()
        );
    }
    println!("xtask eval — eval gauntlet exited 0; manifest at {}", output.display());
    Ok(())
}

// ----- conformance subcommand (Plan 05-02 / 05-03) -----

fn cmd_conformance(backend: &str) -> Result<()> {
    let test_name = format!("run_storage_{backend}");
    println!("xtask conformance — invoking cargo test -p lunaris-conformance --test {test_name}");
    let status = Command::new("cargo")
        .args(["test", "-p", "lunaris-conformance", "--test", &test_name, "--", "--nocapture"])
        .status()
        .with_context(|| format!("invoke cargo test conformance backend={backend}"))?;
    if !status.success() {
        anyhow::bail!(
            "lunaris-conformance test {test_name} exited with status {:?}",
            status.code()
        );
    }
    Ok(())
}

// ----- bench subcommand -----

fn cmd_bench(skip_bench: bool, groups: &str) -> Result<()> {
    let groups: Vec<&str> = groups.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if groups.is_empty() {
        anyhow::bail!("no bench groups specified");
    }

    println!("Phase 2 Latency Gauntlet");
    println!("Backends: MOON_URL / PG_URL (each backend skips when env unset OR unreachable)");
    println!("Groups: {}", groups.join(", "));
    println!("Expected wall-clock per backend: ~30 s ingest + ~25 s recall + ~20 s atomic_write");
    println!();

    if !skip_bench {
        for group in &groups {
            println!("→ cargo bench -p lunaris-bench --bench {group}");
            let status = Command::new("cargo")
                .args(["bench", "-p", "lunaris-bench", "--bench", group])
                .status()
                .with_context(|| format!("invoke cargo bench {group}"))?;
            if !status.success() {
                eprintln!(
                    "WARN: cargo bench {group} returned exit code {:?}; continuing to print table from any data that DID land",
                    status.code()
                );
            }
        }
    }

    let report = collect_report(&groups)?;
    print_delta_table(&report);
    let exit_code = decide_exit_code(&report);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

// ----- Phase 2 budget table (mirror of tests/budget_assertions.rs) -----
//
// Kept in sync by hand — Plan 02-04 ships the delta runner separately from
// the budget assertion test so they don't share a module. A future plan can
// extract the table into a shared lunaris-bench::budget submodule.

#[derive(Debug, Clone, Copy)]
struct Row {
    group: &'static str,
    bench: &'static str,
    p50_budget_ms: f64,
    p99_budget_ms: f64,
}

const ROWS: &[Row] = &[
    Row {
        group: "ingest_hot_path",
        bench: "ingest_12kb_md",
        p50_budget_ms: 50.0,
        p99_budget_ms: 110.0,
    },
    Row { group: "recall_hot_path", bench: "recall_q", p50_budget_ms: 25.0, p99_budget_ms: 80.0 },
    Row {
        group: "atomic_write_hot_path",
        bench: "atomic_write",
        p50_budget_ms: 3.0,
        p99_budget_ms: 12.0,
    },
    // Plan 03-04 — INGEST-06 graph-on row (blueprint §4.1: 300 ms p50 / 570 ms p99).
    // Per Plan 02-04 decision (table-mirror): explicit duplication with
    // tests/budget_assertions.rs::BUDGET_TABLE rather than a shared
    // submodule (table is small + change-rare; extracting forces xtask to
    // take a path dep on lunaris-bench).
    //
    // Schema-W-6: ONE row per (group, bench); renderer (read_per_backend)
    // fans across moon + postgres internally — do NOT add a separate
    // Postgres row here.
    Row {
        group: "ingest_hot_path",
        bench: "ingest_12kb_md_graph_on",
        p50_budget_ms: 300.0,
        p99_budget_ms: 570.0,
    },
];

#[derive(Debug, Default, Clone)]
struct PerBackend {
    p50_ms: Option<f64>,
    p99_ms: Option<f64>,
}

#[derive(Debug)]
struct ReportRow {
    group: &'static str,
    bench: &'static str,
    p50_budget_ms: f64,
    p99_budget_ms: f64,
    moon: PerBackend,
    pg: PerBackend,
}

#[derive(Debug, Default)]
struct Report {
    rows: Vec<ReportRow>,
}

fn collect_report(groups: &[&str]) -> Result<Report> {
    let root = criterion_root();
    let mut report = Report::default();
    for row in ROWS {
        if !groups.contains(&row.group) {
            continue;
        }
        let moon = read_per_backend(&root, row.group, row.bench, "moon");
        let pg = read_per_backend(&root, row.group, row.bench, "postgres");
        report.rows.push(ReportRow {
            group: row.group,
            bench: row.bench,
            p50_budget_ms: row.p50_budget_ms,
            p99_budget_ms: row.p99_budget_ms,
            moon,
            pg,
        });
    }
    Ok(report)
}

fn read_per_backend(root: &Path, group: &str, bench: &str, label: &str) -> PerBackend {
    let dir = root.join(group).join(format!("{bench}/{label}")).join("new");
    let estimates = dir.join("estimates.json");
    let sample = dir.join("sample.json");
    let mut out = PerBackend::default();
    if let Ok(median_ns) = read_median_ns(&estimates) {
        out.p50_ms = Some(median_ns / 1_000_000.0);
    }
    if let Ok(p99_ns) = read_p99_ns(&sample) {
        out.p99_ms = Some(p99_ns / 1_000_000.0);
    }
    out
}

fn read_median_ns(path: &Path) -> Result<f64> {
    let bytes = std::fs::read(path).context("read estimates.json")?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).context("parse estimates.json")?;
    v.get("median")
        .and_then(|m| m.get("point_estimate"))
        .and_then(|pe| pe.as_f64())
        .context("missing .median.point_estimate")
}

fn read_p99_ns(path: &Path) -> Result<f64> {
    let bytes = std::fs::read(path).context("read sample.json")?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).context("parse sample.json")?;

    if let (Some(times), Some(iters)) =
        (v.get("times").and_then(|x| x.as_array()), v.get("iters").and_then(|x| x.as_array()))
        && !times.is_empty()
        && times.len() == iters.len()
    {
        let mut per_iter: Vec<f64> = times
            .iter()
            .zip(iters.iter())
            .filter_map(|(t, n)| {
                let total = t.as_f64()?;
                let nn = n.as_f64()?;
                if nn > 0.0 { Some(total / nn) } else { None }
            })
            .collect();
        return percentile(&mut per_iter, 0.99);
    }

    if let Some(rows) = v.as_array() {
        let mut per_iter: Vec<f64> = rows
            .iter()
            .filter_map(|r| {
                let arr = r.as_array()?;
                if arr.len() != 2 {
                    return None;
                }
                let nn = arr[0].as_f64()?;
                let total = arr[1].as_f64()?;
                if nn > 0.0 { Some(total / nn) } else { None }
            })
            .collect();
        return percentile(&mut per_iter, 0.99);
    }
    anyhow::bail!("sample.json shape not recognised")
}

fn percentile(values: &mut [f64], q: f64) -> Result<f64> {
    if values.is_empty() {
        anyhow::bail!("empty samples");
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((values.len() as f64 - 1.0) * q).round() as usize;
    Ok(values[idx])
}

// ----- table renderer -----

fn print_delta_table(r: &Report) {
    println!();
    println!("Phase 2 Latency Report — Moon vs Postgres");
    println!();

    println!(
        "| {:<32} | {:<10} | {:<10} | {:<10} | {:<10} | {:<8} | {:<8} | {:<7} |",
        "Bench", "Moon p50", "Moon p99", "PG p50", "PG p99", "Δ p50", "Δ p99", "Budget"
    );
    println!(
        "| {:<32} | {:<10} | {:<10} | {:<10} | {:<10} | {:<8} | {:<8} | {:<7} |",
        "-".repeat(32),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(8),
        "-".repeat(8),
        "-".repeat(7),
    );

    let mut moon_misses: Vec<String> = Vec::new();
    let mut pg_misses: Vec<String> = Vec::new();
    let mut soft_misses: Vec<String> = Vec::new();
    let mut moon_seen = false;
    let mut pg_seen = false;

    for row in &r.rows {
        if row.moon.p50_ms.is_some() {
            moon_seen = true;
        }
        if row.pg.p50_ms.is_some() {
            pg_seen = true;
        }

        let bench_id = format!("{}/{}", row.group, row.bench);
        let moon_p50 = row.moon.p50_ms.map(|x| format!("{x:.1} ms")).unwrap_or_else(|| "-".into());
        let moon_p99 = row.moon.p99_ms.map(|x| format!("{x:.1} ms")).unwrap_or_else(|| "-".into());
        let pg_p50 = row.pg.p50_ms.map(|x| format!("{x:.1} ms")).unwrap_or_else(|| "-".into());
        let pg_p99 = row.pg.p99_ms.map(|x| format!("{x:.1} ms")).unwrap_or_else(|| "-".into());

        let delta_p50 = match (row.moon.p50_ms, row.pg.p50_ms) {
            (Some(m), Some(p)) if m > 0.0 => format!("{:+.0}%", (p / m - 1.0) * 100.0),
            _ => "-".into(),
        };
        let delta_p99 = match (row.moon.p99_ms, row.pg.p99_ms) {
            (Some(m), Some(p)) if m > 0.0 => format!("{:+.0}%", (p / m - 1.0) * 100.0),
            _ => "-".into(),
        };
        let budget = format!("{:.0}/{:.0}", row.p50_budget_ms, row.p99_budget_ms);

        println!(
            "| {bench_id:<32} | {moon_p50:<10} | {moon_p99:<10} | {pg_p50:<10} | {pg_p99:<10} | {delta_p50:<8} | {delta_p99:<8} | {budget:<7} |"
        );

        // Budget violation tracking.
        if let Some(p50) = row.moon.p50_ms
            && p50 > row.p50_budget_ms
        {
            moon_misses.push(format!("{bench_id} p50 {p50:.1} ms > {:.1} ms", row.p50_budget_ms));
        }
        if let Some(p99) = row.moon.p99_ms
            && row.p99_budget_ms > 0.0
            && p99 > row.p99_budget_ms
        {
            moon_misses.push(format!("{bench_id} p99 {p99:.1} ms > {:.1} ms", row.p99_budget_ms));
        }
        if let Some(p50) = row.pg.p50_ms
            && p50 > row.p50_budget_ms
        {
            let overshoot = p50 / row.p50_budget_ms;
            let line = format!("{bench_id} p50 {p50:.1} ms > {:.1} ms", row.p50_budget_ms);
            if overshoot > 2.0 {
                soft_misses.push(format!("{line} (>2x — soft-fail per ROADMAP)"));
            } else {
                pg_misses.push(line);
            }
        }
        if let Some(p99) = row.pg.p99_ms
            && row.p99_budget_ms > 0.0
            && p99 > row.p99_budget_ms
        {
            let overshoot = p99 / row.p99_budget_ms;
            let line = format!("{bench_id} p99 {p99:.1} ms > {:.1} ms", row.p99_budget_ms);
            if overshoot > 2.0 {
                soft_misses.push(format!("{line} (>2x — soft-fail per ROADMAP)"));
            } else {
                pg_misses.push(line);
            }
        }
    }

    println!();
    if !moon_seen {
        println!("(!) Moon: no Criterion data found — set MOON_URL and re-run `cargo xtask bench`");
    } else if moon_misses.is_empty() {
        println!("[OK] Moon:    all budgets met");
    } else {
        println!("[FAIL] Moon: {} budget miss(es)", moon_misses.len());
        for m in &moon_misses {
            println!("       - {m}");
        }
    }

    if !pg_seen {
        println!(
            "(!) Postgres: no Criterion data found — set PG_URL and re-run `cargo xtask bench`"
        );
    } else if pg_misses.is_empty() && soft_misses.is_empty() {
        println!("[OK] Postgres: all budgets met");
    } else {
        if !pg_misses.is_empty() {
            println!("[FAIL] Postgres: {} budget miss(es)", pg_misses.len());
            for m in &pg_misses {
                println!("       - {m}");
            }
        }
        if !soft_misses.is_empty() {
            println!(
                "[SOFT] Postgres: {} budget miss(es) > 2x — ship Phase 2 on Moon-only per ROADMAP risk register",
                soft_misses.len()
            );
            for m in &soft_misses {
                println!("       - {m}");
            }
        }
    }
    println!();
}

fn decide_exit_code(r: &Report) -> i32 {
    // Moon over budget → fail loud (Phase 2 ship blocker).
    for row in &r.rows {
        if let Some(p50) = row.moon.p50_ms
            && p50 > row.p50_budget_ms
        {
            return 1;
        }
        if let Some(p99) = row.moon.p99_ms
            && row.p99_budget_ms > 0.0
            && p99 > row.p99_budget_ms
        {
            return 1;
        }
    }
    // Postgres-only misses → exit 0 (with the table already showing
    // [FAIL]/[SOFT] markers for ops to triage).
    0
}

fn criterion_root() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    PathBuf::from(target).join("criterion")
}
