//! Plan 05-06 EVAL-01..09 — single-binary eval gauntlet entry.
//!
//! Per CONTEXT.md D-19: 7 subcommands (`longmemeval | locomo | er-f1 |
//! memory | e2e | perf-delta | all`). Each subcommand runs the corresponding
//! harness, appends one or more `EvalRow` to a shared `Vec`, then writes
//! `eval-results.json` per D-20.
//!
//! ## Exit codes
//!
//! - `0` — every harness PASS or SKIPPED (no FAIL rows). CI proceeds.
//! - `1` — one or more harness FAILED OR write/run error. CI blocks merge
//!   per `! grep -q '"status":"FAIL"' eval-results.json`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "lunaris-evals",
    version,
    about = "Lunaris eval gauntlet (EVAL-01..09)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: EvalCmd,

    /// Output JSON manifest path (default `eval-results.json` at workspace
    /// root). Marked `global` so the flag works either before or after the
    /// subcommand: `lunaris-evals all --output X` AND
    /// `lunaris-evals --output X all` both parse identically.
    #[arg(
        long,
        default_value = "eval-results.json",
        env = "LUNARIS_EVAL_OUTPUT",
        global = true
    )]
    output: std::path::PathBuf,
}

#[derive(Subcommand, Debug)]
enum EvalCmd {
    /// EVAL-01 — LongMemEval J-score (≥65 alpha bar).
    Longmemeval,
    /// EVAL-02 — LoCoMo J-score (≥70 alpha bar).
    Locomo,
    /// EVAL-03 — Entity Resolution F1 (≥0.80).
    ErF1,
    /// EVAL-04 — RSS at 1M facts on Moon TQ4 4-bit (≤2 GB).
    Memory,
    /// EVAL-05 + EVAL-06 — End-to-end Helios scenarios (chat 10k + doc 50k).
    E2e,
    /// EVAL-07 / EVAL-09 — Moon vs Postgres p50 ratio from Criterion JSON.
    PerfDelta,
    /// EVAL-01..09 — Run every harness; aggregate into one eval-results.json.
    All,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Best-effort tracing init — chaos.rs convention. Swallow any failure
    // (a parent test may have already installed a subscriber).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let mut results: Vec<lunaris_bench::eval::EvalRow> = Vec::new();
    let outcome = match cli.cmd {
        EvalCmd::Longmemeval => lunaris_bench::eval::longmemeval::run(&mut results).await,
        EvalCmd::Locomo => lunaris_bench::eval::locomo::run(&mut results).await,
        EvalCmd::ErF1 => lunaris_bench::eval::er_f1::run(&mut results).await,
        EvalCmd::Memory => lunaris_bench::eval::memory::run(&mut results).await,
        EvalCmd::E2e => lunaris_bench::eval::e2e::run(&mut results).await,
        EvalCmd::PerfDelta => lunaris_bench::eval::perf_delta::run(&mut results).await,
        EvalCmd::All => lunaris_bench::eval::run_all(&mut results).await,
    };

    if let Err(e) = outcome {
        eprintln!("eval-gauntlet error: {e}");
        return ExitCode::from(1);
    }

    if let Err(e) = lunaris_bench::eval::results::write_json(&cli.output, &results) {
        eprintln!("eval-results write to {} failed: {e}", cli.output.display());
        return ExitCode::from(1);
    }

    lunaris_bench::eval::results::print_markdown_summary(&results);

    let any_fail = results.iter().any(|r| r.status == "FAIL");
    if any_fail {
        eprintln!(
            "eval-gauntlet: at least one harness FAILED — see {}",
            cli.output.display()
        );
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
