//! `lunaris-codegen` — Plan 08-01 CLI.
//!
//! Four modes, keyed by clap-derived flags:
//!
//! - `--emit {py,ts}` — regenerate and write snapshots to
//!   `crates/lunaris-codegen/snapshots/`. Idempotent; two consecutive runs
//!   produce byte-identical bytes on disk.
//! - `--check` — regenerate into memory and diff against the committed
//!   snapshots. Exit 0 when clean, 1 with a unified diff on stderr on
//!   drift.
//! - `--dump-ir` — print the extracted [`SurfaceIR`] as pretty JSON to
//!   stdout and exit 0. Used by the golden-snapshot commit pipeline.
//! - `--workspace-root <path>` — override the default workspace root
//!   (`CARGO_MANIFEST_DIR/../..`). Used by the integration tests.
//!
//! The `parity-check` CI job in `.github/workflows/ci.yml` runs this binary
//! with `--check` on every PR.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use lunaris_codegen::{check_surface, extract_surface};

#[derive(Parser, Debug)]
#[command(
    name = "lunaris-codegen",
    about = "Single-source PyO3 + napi-rs binding generator for Lunaris (Phase 8 Plan 08-01).",
    version
)]
struct Args {
    /// Emit target(s). Repeatable: `--emit py --emit ts`.
    #[arg(long = "emit", value_enum)]
    emit: Vec<EmitTarget>,
    /// Diff regenerated output against committed snapshots; exit 1 on drift.
    #[arg(long = "check")]
    check: bool,
    /// Print the extracted SurfaceIR as pretty JSON to stdout and exit 0.
    /// Feeds the golden-snapshot commit pipeline.
    #[arg(long = "dump-ir")]
    dump_ir: bool,
    /// Workspace root override. Defaults to `CARGO_MANIFEST_DIR/../..`.
    #[arg(long = "workspace-root")]
    workspace_root: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum EmitTarget {
    Py,
    Ts,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = args
        .workspace_root
        .clone()
        .unwrap_or_else(default_workspace_root);
    let ir = extract_surface(&root)
        .with_context(|| format!("lunaris-codegen: extract_surface({})", root.display()))?;

    if args.dump_ir {
        println!("{}", serde_json::to_string_pretty(&ir)?);
        return Ok(());
    }

    if args.check {
        let outcome = check_surface(&root, &ir)?;
        if outcome.is_clean() {
            println!("lunaris-codegen: --check clean ({} paths matched)", lunaris_codegen::codegen_managed_paths(&ir).len());
            return Ok(());
        }
        for drift in &outcome.drift {
            eprintln!("--- drift in {} ---", drift.path.display());
            eprintln!("{}", drift.diff);
        }
        eprintln!(
            "lunaris-codegen: --check FAILED — {} path(s) drifted from committed snapshots.",
            outcome.drift.len()
        );
        eprintln!(
            "hint: run `cargo run -p lunaris-codegen -- --emit py --emit ts` and commit the result."
        );
        std::process::exit(1);
    }

    // Default: --emit mode. An empty --emit list is treated as "no-op" so
    // a bare `cargo run -p lunaris-codegen` is safe.
    if args.emit.is_empty() {
        eprintln!(
            "lunaris-codegen: no mode specified; pass --emit py, --emit ts, --check, or --dump-ir"
        );
        return Ok(());
    }
    // For simplicity (and because the snapshots live in the same crate)
    // we always rewrite ALL managed snapshots when any `--emit` target is
    // specified. This keeps `--check` honest: a future `--emit py`-only
    // run cannot leave `generated_ts.rs` stale.
    let _ = &args.emit;
    lunaris_codegen::check::write_snapshots(&root, &ir)?;
    println!(
        "lunaris-codegen: wrote {} snapshot(s) under {}/crates/lunaris-codegen/snapshots/",
        lunaris_codegen::codegen_managed_paths(&ir).len(),
        root.display()
    );
    Ok(())
}

fn default_workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/lunaris-codegen/`; ../../ is the
    // workspace root. When the binary is invoked outside of `cargo run`
    // (e.g. via an installed binary) callers pass `--workspace-root`
    // explicitly.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
