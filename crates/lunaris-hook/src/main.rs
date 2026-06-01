//! `lunaris-hook` — proactive capture of Claude Code lifecycle events.
//!
//! Reads one hook envelope from stdin (to EOF), writes one Episode via
//! ScopedLunaris::ingest, exits with a sysexits.h-style code:
//!   0  = success (Episode written) OR unknown event kind (forward-compat no-op)
//!        OR emergency-drop (ingest stalled beyond timeout budget)
//!   64 = parse error
//!   65 = ingest error
//!   66 = filter-rejected (path/kind/size policy denied the event)
//!   73 = internal error (scope derivation, storage open)
//!
//! # Emergency-drop (HOOK-06)
//!
//! The ingest call is wrapped in `tokio::time::timeout(LUNARIS_HOOK_DROP_AFTER_MS)`.
//! If storage stalls beyond the budget, the hook emits a single-line JSON warning
//! to stderr and exits 0. This guarantees the hook never blocks Claude Code
//! indefinitely under storage-degradation conditions.
//!
//! Env var:
//!   LUNARIS_HOOK_DROP_AFTER_MS  – timeout in ms (default 100, clamped 10–10000)
//!
//! Emergency-drop warning format (single JSON line to stderr):
//!   {"level":"warn","event":"emergency_drop","reason":"ingest_timeout_<N>ms","scope":"...","kind":"..."}

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use clap::Parser;

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
    if let Err(e) = std::io::stdin().take(4 * 1024 * 1024).read_to_end(&mut stdin_bytes) {
        let err_json = serde_json::json!({
            "error": "stdin_read_failed",
            "message": e.to_string(),
        });
        eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
        return 64;
    }

    // Extract the event kind string BEFORE the timeout block. This is a cheap
    // JSON value parse — we need the kind for the emergency_drop warning JSON
    // even if the full typed parse inside `run()` would have failed.
    let kind_str = serde_json::from_slice::<serde_json::Value>(&stdin_bytes)
        .ok()
        .and_then(|v| v.get("hook_event_name").and_then(|k| k.as_str()).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    // Derive cwd for scope resolution.
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            let err_json =
                serde_json::json!({"error": "cwd_unavailable", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 73;
        }
    };

    // Resolve scope + storage URL.
    let scope = match lunaris_hook::scope::resolve(&cwd) {
        Ok(s) => s,
        Err(e) => {
            let err_json =
                serde_json::json!({"error": "scope_resolution_failed", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 73;
        }
    };

    let storage_url = match lunaris_hook::scope::resolve_storage_url(&scope) {
        Ok(u) => u,
        Err(e) => {
            let err_json =
                serde_json::json!({"error": "storage_url_failed", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 73;
        }
    };

    let storage = match lunaris::open(&storage_url).await {
        Ok(s) => s,
        Err(e) => {
            let err_json =
                serde_json::json!({"error": "lunaris_open_failed", "message": e.to_string()});
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
            return 65;
        }
    };

    // Read LUNARIS_HOOK_DROP_AFTER_MS (default 100ms, clamped 10–10000).
    // This is the ingest timeout budget — if storage stalls beyond this, the
    // hook emits a warning and exits 0 (emergency-drop, HOOK-06).
    let drop_ms = std::env::var("LUNARIS_HOOK_DROP_AFTER_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(100)
        .clamp(10, 10_000);
    let drop_timeout = tokio::time::Duration::from_millis(drop_ms);

    // Capture scope string before moving into the async block.
    let scope_str = scope.as_str().to_owned();

    // Wrap the ingest call in tokio::time::timeout (emergency-drop, HOOK-06).
    //
    // IMPORTANT: The stall is INSIDE the async block so it counts against the
    // timeout budget. LUNARIS_TEST_STALL_MS is a test-only injection point;
    // it is undocumented in user-facing docs and has no effect in production
    // (env var is absent in normal operation).
    let result = tokio::time::timeout(drop_timeout, async {
        // Test-only stall injection: LUNARIS_TEST_STALL_MS env var.
        // NOT production behavior — used only by the emergency_drop test to
        // deterministically trip the timeout at a known threshold.
        // The sleep is a tokio::time::sleep so tokio::time::timeout can cancel
        // it at the .await point.
        #[cfg(debug_assertions)]
        if let Ok(stall_str) = std::env::var("LUNARIS_TEST_STALL_MS") {
            if let Ok(stall_ms) = stall_str.parse::<u64>() {
                tokio::time::sleep(tokio::time::Duration::from_millis(stall_ms)).await;
            }
        }

        lunaris_hook::run_with_storage(&stdin_bytes, scope, storage).await
    })
    .await;

    match result {
        Ok(Ok(Some(lsn))) => {
            tracing::debug!(lsn = %lsn, "episode written");
            0
        }
        Ok(Ok(None)) => {
            // Unknown event kind — intentional no-op (B2 fix: exit 0, not 66).
            // Exit 66 is reserved for filter-rejected events.
            tracing::info!("unknown event kind — no-op, exiting 0");
            0
        }
        Ok(Err(e)) => {
            // Ingest error within timeout — normal error path (exit 64/65/66).
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
        Err(_timeout) => {
            // Emergency drop: ingest stalled beyond the budget.
            // Exit 0 to not block the Claude Code tool call.
            // Emit a single-line JSON warning so operators can detect storage degradation.
            // T-24-04-01: scope name and kind are visible in this line — operators
            // MUST NOT route hook stderr to external sinks without sanitization.
            let warn_json = serde_json::json!({
                "level": "warn",
                "event": "emergency_drop",
                "reason": format!("ingest_timeout_{}ms", drop_ms),
                "scope": scope_str,
                "kind": kind_str,
            });
            eprintln!("{}", serde_json::to_string(&warn_json).unwrap_or_default());
            0
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
