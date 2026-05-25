//! `lunaris-hook` — proactive capture of Claude Code lifecycle events.
//!
//! Reads one hook envelope from stdin (to EOF), writes one Episode via
//! ScopedLunaris::ingest, exits with a sysexits.h-style code:
//!   0  = success (Episode written) OR unknown event kind (forward-compat no-op)
//!   64 = parse error
//!   65 = ingest error
//!   66 = Phase 24 reserved (filter-rejected; NOT used in Phase 23)
//!   73 = internal error (scope derivation, storage open)

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use clap::Parser;
use lunaris::Lunaris;

#[derive(Parser, Debug)]
#[command(
    name = "lunaris-hook",
    version,
    about = "Lunaris hook adapter — capture Claude Code lifecycle events into agent memory"
)]
struct Cli {
    /// Log filter directive (RUST_LOG syntax). Logs go to stderr.
    #[arg(long, env = "LUNARIS_HOOK_LOG", default_value = "warn")]
    log_level: String,
}

fn main() {
    // tokio current-thread: consistent with lunaris-mcp; keeps cold-start cheap.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime init");
    let exit_code = rt.block_on(async_main());
    std::process::exit(exit_code);
}

async fn async_main() -> i32 {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);

    // Read stdin to EOF — Claude Code fires one event per process invocation.
    // Cap at 4 MiB to prevent oversized-stdin DoS (T-23-02-01 mitigation).
    let mut stdin_bytes = Vec::new();
    use std::io::Read;
    if let Err(e) = std::io::stdin()
        .take(4 * 1024 * 1024)
        .read_to_end(&mut stdin_bytes)
    {
        let err_json = serde_json::json!({
            "error": "stdin_read_failed",
            "message": e.to_string(),
        });
        eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
        return 64;
    }

    // Derive cwd for scope resolution.
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            let err_json = serde_json::json!({"error": "cwd_unavailable", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 73;
        }
    };

    // Resolve scope + storage URL.
    let scope = match lunaris_hook::scope::resolve(&cwd) {
        Ok(s) => s,
        Err(e) => {
            let err_json = serde_json::json!({"error": "scope_resolution_failed", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 73;
        }
    };

    let storage_url = match lunaris_hook::scope::resolve_storage_url(&scope) {
        Ok(u) => u,
        Err(e) => {
            let err_json = serde_json::json!({"error": "storage_url_failed", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 73;
        }
    };

    let lunaris = match Lunaris::open(&storage_url).await {
        Ok(l) => Arc::new(l),
        Err(e) => {
            let err_json = serde_json::json!({"error": "lunaris_open_failed", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 65;
        }
    };

    match lunaris_hook::run(&stdin_bytes, scope, lunaris).await {
        Ok(Some(lsn)) => {
            tracing::debug!(lsn = %lsn, "episode written");
            0
        }
        Ok(None) => {
            // Unknown event kind — intentional no-op (B2 fix: exit 0, not 66).
            // Exit 66 is reserved for Phase 24 filter-rejected events.
            tracing::info!("unknown event kind — no-op, exiting 0");
            0
        }
        Err(e) => {
            let log_json = std::env::var("LUNARIS_HOOK_LOG_JSON").is_ok_and(|v| v == "1");
            if log_json {
                let err_json = serde_json::json!({
                    "error": e.to_string(),
                    "exit_code": e.exit_code(),
                });
                eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            } else {
                eprintln!("lunaris-hook error: {e}");
            }
            e.exit_code()
        }
    }
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
