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

use lunaris_server::{Command, Config, OpsCli, Shutdown};

#[tokio::main]
async fn main() -> ExitCode {
    // Plan 05-05 Task 3 — REPLACES the Plan 05-01 minimal subscriber init with
    // `lunaris::logging::init()` (the OPS-08 JSON-vs-pretty selector helper
    // per CONTEXT.md D-26). JSON when `LUNARIS_ENV=production` OR
    // `!std::io::stdout().is_terminal()`; pretty otherwise. Idempotent via
    // try_init().ok() so test code with its own subscriber doesn't panic.
    lunaris::logging::init();

    // Dispatch: a recognised operational subcommand (`migrate`, `bootstrap-db`)
    // → run it and exit; anything else → the unchanged serve path. See
    // `config.rs` for why this is a manual peek rather than one clap struct.
    if std::env::args().nth(1).is_some_and(|a| Command::is_subcommand_name(&a)) {
        return match OpsCli::parse().command {
            Command::Migrate { storage } => run_migrate(&storage).await,
            Command::BootstrapDb { admin_url, app_role, app_password } => {
                run_bootstrap_db(&admin_url, &app_role, &app_password).await
            }
        };
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

/// `lunaris-server migrate --storage <admin_url>` — apply every embedded
/// Postgres migration, then exit. No `sqlx-cli`, no checked-out migrations dir.
async fn run_migrate(storage: &str) -> ExitCode {
    match lunaris::PostgresStorage::migrate(storage).await {
        Ok(()) => {
            tracing::info!(storage = %redact(storage), "migrations applied");
            println!("ok: migrations applied at {}", redact(storage));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("migrate failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// `lunaris-server bootstrap-db --admin-url … --app-role … --app-password …` —
/// migrate as admin, create/repair the `NOSUPERUSER NOBYPASSRLS` app role +
/// grants, then report any RLS hardening gaps. Replaces the §6.2 recipe.
async fn run_bootstrap_db(admin_url: &str, app_role: &str, app_password: &str) -> ExitCode {
    match lunaris::bootstrap_app_role(admin_url, app_role, app_password).await {
        Ok(report) => {
            println!("ok: migrations applied; role {app_role:?} created/updated with DML grants");
            for t in &report.tables_missing_force_rls {
                eprintln!("warning: table {t:?} has RLS enabled but FORCE ROW LEVEL SECURITY off");
            }
            for (p, t) in &report.policies_missing_with_check {
                eprintln!(
                    "warning: policy {p:?} on {t:?} is write-capable but has no WITH CHECK clause"
                );
            }
            if report.is_clean() {
                println!("rls: all tenant tables FORCE-enabled and write policies have WITH CHECK");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bootstrap-db failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// Redact `user:pass@` userinfo from a connection string for logging.
fn redact(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("***");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => url.to_string(),
    }
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
