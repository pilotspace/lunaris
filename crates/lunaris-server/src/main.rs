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

    // Plan 05-05 Task 3 will REPLACE this call site with `lunaris::logging::init()`
    // (the JSON / pretty selector helper). Until then, a minimal env-filter
    // init keeps tracing output usable.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .ok();

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
