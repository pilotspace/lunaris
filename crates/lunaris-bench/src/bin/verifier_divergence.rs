//! RFC 0006 §4 — verifier divergence capture binary.
//!
//! Loads the two Candle verifiers (27B + 270M) from their default cache
//! paths, runs `lunaris_verify::divergence::compare_verifiers` over the
//! deterministic 100-item corpus from `lunaris_bench::verifier_corpus`,
//! prints the divergence rate, and exits 0 (PASS — divergence ≤ 0.05)
//! or 1 (FAIL — divergence > 0.05).
//!
//! ## Gating
//!
//! Compiled only when BOTH `verify-small` AND `verify-large` are enabled
//! on `lunaris-verify`. Without both backends the comparison is
//! ill-defined. The Cargo.toml [[bin]] entry adds `required-features` so
//! a bare `cargo build -p lunaris-bench` succeeds (the binary just
//! doesn't get built).
//!
//! ## Run
//!
//! ```bash
//! # Pre-flight: weights present?
//! ls ~/.cache/lunaris/models/gemma-3-27b-it     # ~14 GiB
//! ls ~/.cache/lunaris/models/gemma-3-270m-it    # ~600 MB
//!
//! # Capture the number:
//! cargo run --release \
//!     --bin verifier-divergence \
//!     -p lunaris-bench \
//!     --features verify-small,verify-large
//! ```
//!
//! Output (`docs/benchmarks/v0.2.x/verifier-divergence.md` format):
//!
//! ```text
//! corpus_size: 100
//! matches: 96
//! diverged: 4
//! divergence_rate: 0.04
//! threshold: 0.05
//! verdict: PASS
//! ```

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::process::ExitCode;
use std::sync::Arc;

use lunaris_bench::verifier_corpus::{RFC_0006_CORPUS_SIZE, verifier_corpus_100};
use lunaris_verify::{
    CandleGemma3_27B, CandleGemma3_27BOpts, CandleGemma3_270M, CandleGemma3_270MOpts, Verifier,
    divergence::compare_verifiers,
};

/// RFC 0006 §4 default-flip pass threshold. Divergence ≤ this passes;
/// > this fails.
const PASS_THRESHOLD: f64 = 0.05;

#[tokio::main]
async fn main() -> ExitCode {
    // Init tracing so the Candle loaders' diagnostics surface to stderr.
    // We use a plain `fmt` subscriber here rather than `lunaris::logging::init`
    // because this binary doesn't depend on the umbrella crate.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    tracing::info!(
        corpus_size = RFC_0006_CORPUS_SIZE,
        threshold = PASS_THRESHOLD,
        "RFC 0006 §4 verifier divergence capture starting"
    );

    let verifier_a = match load_27b().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: failed to load CandleGemma3_27B: {e}");
            eprintln!("Install weights via:");
            eprintln!("  huggingface-cli download google/gemma-3-27b-it \\");
            eprintln!("    --local-dir ~/.cache/lunaris/models/gemma-3-27b-it");
            return ExitCode::from(2);
        }
    };
    tracing::info!("CandleGemma3_27B loaded");

    let verifier_b = match load_270m().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERROR: failed to load CandleGemma3_270M: {e}");
            eprintln!("Install weights via:");
            eprintln!("  huggingface-cli download google/gemma-3-270m-it \\");
            eprintln!("    --local-dir ~/.cache/lunaris/models/gemma-3-270m-it");
            return ExitCode::from(2);
        }
    };
    tracing::info!("CandleGemma3_270M loaded");

    let corpus = verifier_corpus_100();
    tracing::info!(items = corpus.len(), "running compare_verifiers");

    let report = match compare_verifiers(verifier_a, verifier_b, corpus).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ERROR: compare_verifiers failed: {e}");
            return ExitCode::from(3);
        }
    };

    let verdict = if report.passes(PASS_THRESHOLD) { "PASS" } else { "FAIL" };

    // Markdown-friendly output, intended to be appended to
    // docs/benchmarks/v0.2.x/verifier-divergence.md.
    println!("corpus_size: {}", report.total);
    println!("matches: {}", report.matches);
    println!("diverged: {}", report.diverged);
    println!("divergence_rate: {:.4}", report.divergence_rate);
    println!("threshold: {PASS_THRESHOLD}");
    println!("verdict: {verdict}");

    if !report.passes(PASS_THRESHOLD) {
        // Print the diverging cases so the operator can investigate.
        eprintln!();
        eprintln!("Diverging cases ({} of {}):", report.diverged, report.total);
        for (i, case) in report.cases.iter().enumerate().filter(|(_, c)| c.diverged) {
            eprintln!(
                "  [{i}] decision_a.applies={} winner_a={:?} loser_a={:?}",
                case.decision_a.applies(),
                case.decision_a.winner_id,
                case.decision_a.loser_id,
            );
            eprintln!(
                "      decision_b.applies={} winner_b={:?} loser_b={:?}",
                case.decision_b.applies(),
                case.decision_b.winner_id,
                case.decision_b.loser_id,
            );
        }
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

async fn load_27b() -> Result<Arc<dyn Verifier>, lunaris_core::LunarisError> {
    let v = CandleGemma3_27B::new(CandleGemma3_27BOpts::default()).await?;
    Ok(Arc::new(v) as Arc<dyn Verifier>)
}

async fn load_270m() -> Result<Arc<dyn Verifier>, lunaris_core::LunarisError> {
    let v = CandleGemma3_270M::new(CandleGemma3_270MOpts::default()).await?;
    Ok(Arc::new(v) as Arc<dyn Verifier>)
}
