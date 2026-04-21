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

use lunaris_server::{Config, Shutdown};

#[tokio::main]
async fn main() -> ExitCode {
    let cfg = Config::parse();

    // Plan 05-05 Task 3 — REPLACES the Plan 05-01 minimal subscriber init with
    // `lunaris::logging::init()` (the OPS-08 JSON-vs-pretty selector helper
    // per CONTEXT.md D-26). JSON when `LUNARIS_ENV=production` OR
    // `!std::io::stdout().is_terminal()`; pretty otherwise. Idempotent via
    // try_init().ok() so test code with its own subscriber doesn't panic.
    lunaris::logging::init();

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

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.wait().await;
        })
        .await
    {
        tracing::error!(err = %e, "lunaris-server exited with error");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

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
