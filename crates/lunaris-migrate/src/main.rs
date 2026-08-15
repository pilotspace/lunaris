//! `lunaris-migrate` CLI — dry-run by default, lossy contract up front.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use lunaris_core::{Scope, StoragePort};
use lunaris_migrate::{
    LOSSY_CONTRACT, MigrationOptions, ScopeReport, VerifyReport, discover_scopes, migrate_scope,
    verify_scope,
};

/// One-way migration of Lunaris primitives from the SQLite / Postgres backends
/// into Moon. Reports by default; writes only with `--commit --acknowledge-lossy`.
#[derive(Debug, Parser)]
#[command(name = "lunaris-migrate", version, about, long_about = None)]
struct Cli {
    /// Source store: `sqlite:///path/to.db`, `memory://`, or `postgres://…`.
    #[arg(long)]
    from: String,

    /// Destination store. Moon only — this tool has one direction.
    #[arg(long)]
    to: String,

    /// Migrate this scope. Repeatable. Required for a Postgres source, which
    /// cannot enumerate scopes under its RLS boundary.
    #[arg(long = "scope", conflicts_with = "all_scopes")]
    scopes: Vec<String>,

    /// Migrate every scope the source can enumerate.
    #[arg(long)]
    all_scopes: bool,

    /// Report only, write nothing. This is already the default; the flag exists
    /// so a cautious invocation can say so out loud.
    #[arg(long, conflicts_with = "commit")]
    dry_run: bool,

    /// Perform real writes. Requires `--acknowledge-lossy`.
    #[arg(long)]
    commit: bool,

    /// Acknowledge the lossy contract printed at startup.
    #[arg(long)]
    acknowledge_lossy: bool,

    /// `WriteOp`s per destination `atomic_write`.
    #[arg(long, default_value_t = lunaris_migrate::DEFAULT_BATCH_SIZE)]
    batch_size: usize,

    /// Keys to content-compare during verification (`0` = presence only).
    #[arg(long, default_value_t = lunaris_migrate::DEFAULT_SAMPLE)]
    sample: usize,

    /// Skip the post-migration verification pass.
    #[arg(long)]
    no_verify: bool,

    /// Write the re-embed backlog (one JSONL line per key needing a vector).
    #[arg(long, value_name = "PATH")]
    reembed_manifest: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    )
    .init();

    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        // Verification failed: a non-zero exit is the only signal a deploy
        // script will actually notice.
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("lunaris-migrate: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<bool> {
    println!("{LOSSY_CONTRACT}\n");

    let opts = MigrationOptions {
        commit: cli.commit,
        acknowledge_lossy: cli.acknowledge_lossy,
        batch_size: cli.batch_size.max(1),
        verify: !cli.no_verify,
        sample: cli.sample,
        reembed_manifest: cli.reembed_manifest.clone(),
    };
    if cli.commit && !cli.acknowledge_lossy {
        bail!("{}", lunaris_migrate::ACK_REQUIRED);
    }
    if !opts.writes_enabled() {
        println!("MODE: dry run — counting only, nothing will be written.\n");
    } else {
        println!("MODE: COMMIT — the destination will be written.\n");
    }

    let source = open_source(&cli.from).await?;
    let dest = open_dest(&cli.to).await?;

    let scopes = resolve_scopes(&cli, source.as_ref()).await?;
    if scopes.is_empty() {
        println!("no scopes to migrate");
        return Ok(true);
    }

    let mut all_ok = true;
    for scope in &scopes {
        let report = migrate_scope(source.as_ref(), dest.as_ref(), scope, &opts).await?;
        print_scope_report(&report, opts.writes_enabled());
        if opts.verify && opts.writes_enabled() {
            let v = verify_scope(source.as_ref(), dest.as_ref(), scope, opts.sample).await?;
            print_verify_report(&v);
            all_ok &= v.ok();
        }
    }
    if opts.writes_enabled() {
        println!(
            "\nNEXT STEP: recall on the destination stays empty until vectors are \
             regenerated. Re-embed the keys listed by --reembed-manifest before \
             cutting traffic over."
        );
    }
    Ok(all_ok)
}

async fn resolve_scopes(cli: &Cli, source: &dyn StoragePort) -> Result<Vec<Scope>> {
    if !cli.scopes.is_empty() {
        return cli
            .scopes
            .iter()
            .map(|s| Scope::new(s).with_context(|| format!("invalid --scope {s:?}")))
            .collect();
    }
    if !cli.all_scopes {
        bail!("specify --scope <scope> (repeatable) or --all-scopes");
    }
    discover_scopes(source).await.context(
        "--all-scopes needs a source that can enumerate scopes; Postgres cannot \
         (its RLS boundary forbids a cross-scope scan) — pass explicit --scope arguments",
    )
}

async fn open_source(url: &str) -> Result<Arc<dyn StoragePort>> {
    let scheme = url.split(':').next().unwrap_or_default();
    match scheme {
        "memory" | "sqlite" => {
            let s = lunaris_storage_embedded::EmbeddedStorage::connect(url)
                .await
                .with_context(|| format!("open source {url}"))?;
            Ok(Arc::new(s))
        }
        // `connect_no_migrate` on purpose: a migration SOURCE is read-only, and
        // running DDL against the store an operator is leaving is not this
        // tool's business.
        "postgres" | "postgresql" => {
            let s = lunaris_storage_postgres::PostgresStorage::connect_no_migrate(url)
                .await
                .with_context(|| format!("open source {url}"))?;
            Ok(Arc::new(s))
        }
        "moon" => bail!("Moon is the destination of this tool, not a source"),
        other => bail!("unsupported source scheme {other:?}: use sqlite://, memory://, postgres://"),
    }
}

async fn open_dest(url: &str) -> Result<Arc<dyn StoragePort>> {
    if !url.starts_with("moon://") {
        bail!("destination must be a moon:// URL (this tool migrates INTO Moon)");
    }
    // Plain `connect`: the multi-shard guard and the Moon server-version
    // handshake run here, and there is deliberately no way to skip them.
    let s = lunaris_storage_moon::MoonStorage::connect(url)
        .await
        .with_context(|| format!("open destination {url}"))?;
    Ok(Arc::new(s))
}

fn print_scope_report(r: &ScopeReport, committing: bool) {
    println!("scope {}", r.scope);
    println!("  scanned            {}", r.scanned);
    println!("  eligible           {}", r.eligible);
    println!("  written            {}{}", r.written, if committing { "" } else { "  (dry run)" });
    println!("  skipped: closed valid interval   {}", r.skipped_closed_valid);
    println!("  skipped: closed sys interval     {}", r.skipped_closed_sys);
    println!("  skipped: foreign/malformed key   {}", r.skipped_foreign_key);
    println!("  skipped: superseded sys versions  not enumerable (see contract)");
    println!("  needs re-embed     {}", r.needs_reembed);
    for (kind, n) in &r.by_kind {
        println!("    {kind:<16} {n}");
    }
}

fn print_verify_report(v: &VerifyReport) {
    println!("  verify: source_eligible={} dest_rows={} sampled={}",
             v.source_eligible, v.dest_rows, v.sampled);
    if v.ok() {
        println!("  verify: PASS");
    } else {
        println!("  verify: FAIL missing={} mismatched={}", v.missing, v.mismatched);
        for k in v.missing_examples.iter().chain(v.mismatch_examples.iter()) {
            println!("    {k}");
        }
    }
}
