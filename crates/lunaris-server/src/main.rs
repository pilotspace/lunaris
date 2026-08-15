//! Plan 05-01 — `lunaris-server` binary entry.
//!
//! Mirrors `crates/lunaris-bench/src/bin/chaos.rs` ExitCode + `#[tokio::main]`
//! discipline; clap-derives the `Config` from `lib::config`; constructs ONE
//! `Arc<Lunaris>` via [`lunaris::Lunaris::open`] and shares it across every
//! request handler via `axum::extract::State`.
//!
//! Plan 05-05 will REPLACE the minimal `tracing_subscriber::fmt()` init below
//! with `lunaris::logging::init()` (the JSON / pretty selector helper).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use lunaris_server::{Config, Shutdown, retired_subcommand};

#[tokio::main]
async fn main() -> ExitCode {
    // Plan 05-05 Task 3 — REPLACES the Plan 05-01 minimal subscriber init with
    // `lunaris::logging::init()` (the OPS-08 JSON-vs-pretty selector helper
    // per CONTEXT.md D-26). JSON when `LUNARIS_ENV=production` OR
    // `!std::io::stdout().is_terminal()`; pretty otherwise. Idempotent via
    // try_init().ok() so test code with its own subscriber doesn't panic.
    lunaris::logging::init();

    // 0.7.0 retired the two Postgres-only subcommands (`migrate`,
    // `bootstrap-db`). Peek at argv[1] and, if it names one of them, fail with
    // the migration story rather than letting clap report an unexpected
    // argument — see `config::retired_subcommand`.
    if let Some(msg) = std::env::args().nth(1).and_then(|a| retired_subcommand(&a)) {
        eprintln!("{msg}");
        return ExitCode::from(2);
    }

    let cfg = Config::parse();
    let lunaris = match lunaris::Lunaris::open(&cfg.storage).await {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("Lunaris::open({}) failed: {e}", cfg.storage);
            return ExitCode::from(1);
        }
    };

    let app = lunaris_server::build(cfg.clone(), lunaris.clone());

    let listener = match tokio::net::TcpListener::bind(&cfg.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind({}) failed: {e}", cfg.bind);
            return ExitCode::from(1);
        }
    };
    if let Ok(addr) = listener.local_addr() {
        // Contract for Plan 05-03 protocol-suite ephemeral-port discovery
        // (PATTERNS.md "B-NOTE for planner" under Plan 05-03 subprocess section).
        eprintln!("LISTENING_ON {addr}");
    }
    tracing::info!(bind = %cfg.bind, storage = %cfg.storage, "lunaris-server listening");

    let shutdown = Shutdown::new(cfg.shutdown_grace_secs);
    let drain = shutdown.clone();

    tokio::spawn(async move {
        shutdown_signal().await;
        drain.trigger();
    });

    // Plan 05-05 OPS-06 — spawn the 10s queue-depth poller AFTER the lunaris
    // handle is constructed but BEFORE axum::serve. The poller listens on the
    // SAME `Shutdown::notify()` Arc so SIGTERM cleanly drains it alongside
    // in-flight HTTP requests. The JoinHandle is dropped (the runtime collects
    // the task on shutdown notify).
    let _poller = lunaris_server::queue_depth_poller::spawn_queue_depth_poller(
        lunaris.storage(),
        shutdown.notify(),
    );

    // hotkeys-observability — same lifecycle as the queue-depth poller: 10s
    // ticks feeding `lunaris_hotkey_samples`; unsupported backends warn once
    // and the gauge stays empty.
    let _hotkeys_poller =
        lunaris_server::hotkeys_poller::spawn_hotkeys_poller(lunaris.storage(), shutdown.notify());

    // observability-rollout-maturity — publish lunaris_eval_score{harness} from
    // the last eval run baked into the deployment (LUNARIS_EVAL_RESULTS_PATH) so
    // /metrics reflects it instead of a constant 0. Soft no-op when unset.
    lunaris_server::eval_score::load_eval_scores_from_env();

    // 0.6.2 P0-1 — the drain is BOUNDED by `--shutdown-grace-secs`. The old
    // bare `axum::serve(..).with_graceful_shutdown(..)` never read the flag, so
    // one wedged in-flight request pinned the process until the orchestrator
    // escalated to SIGKILL (which also killed the healthy in-flight requests).
    if let Err(e) = lunaris_server::shutdown::serve_with_deadline(listener, app, shutdown).await {
        tracing::error!(err = %e, "lunaris-server exited with error");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

// `run_migrate` / `run_bootstrap_db` (and the `redact` helper that existed to
// keep Postgres userinfo out of their logs) were deleted in 0.7.0 with the
// Postgres backend. `main` now answers both subcommand names with
// `config::retired_subcommand`.

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received; draining in-flight requests");
}
