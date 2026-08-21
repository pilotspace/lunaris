//! `lunaris` — the human- and script-facing CLI for a running memory store.
//!
//! Why this binary exists: before it, the only ways to ask a Lunaris store a
//! question were the MCP stdio protocol, the Claude Code hook, or the HTTP
//! server. All three are machine surfaces. Debugging the store from a shell
//! meant writing Rust or hand-driving JSON-RPC.
//!
//! It is a **peer** of the other surfaces, not a layer beneath them. The hook
//! and the MCP server keep their in-process path to
//! `lunaris_memory_service::protocol::dispatch`; routing them through a CLI
//! would put a process spawn on a hot path to buy a consistency guarantee the
//! shared dispatch already gives. What a CLI adds is a shell-testable way to
//! ask the SAME dispatch the same question — which makes it the neutral
//! instrument for proving the surfaces agree, instead of a fourth way for them
//! to drift.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

mod corpus;
mod direct;
mod render;
mod request;
mod route;
mod stage;
mod trial;

use std::process::ExitCode;

use clap::Parser;

use request::{Cli, Command};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_trial = matches!(cli.command, Command::Try(_));
    init_tracing(is_trial);

    // `try` is the one subcommand that does not talk to an existing store: it
    // starts its own. Routing it before `to_request` is why `lunaris try` needs
    // no `--scope`, no daemon and no `LUNARIS_STORE_URL`.
    if let Command::Try(args) = &cli.command {
        // The root parser declares `--scope` / `--json` as global flags, so
        // clap will happily accept them here. `try` cannot honour either, and
        // ignoring a flag someone typed is worse than refusing it.
        let globals = trial::Globals {
            scope_on_command_line: trial::scope_flag_typed(std::env::args()),
            json: cli.json,
        };
        return trial::run(args, globals).await;
    }

    let req = match cli.to_request() {
        Ok(req) => req,
        Err(err) => {
            eprintln!("lunaris: {err}");
            return ExitCode::from(2);
        }
    };

    let socket = socket_path();
    let router = route::Router::new(socket);

    match router.dispatch(req).await {
        Ok((value, via)) => {
            if cli.json {
                let envelope = serde_json::json!({ "via": via.as_str(), "data": value });
                println!("{}", serde_json::to_string_pretty(&envelope).unwrap_or_default());
            } else {
                print!("{}", render::render(&value, via));
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lunaris: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Where contextd listens. `LUNARIS_CONTEXTD_SOCKET` wins; otherwise the
/// default under `~/.lunaris`. `None` disables the socket leg entirely, which
/// is what happens with no `$HOME` — the direct path then handles everything.
fn socket_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os(lunaris_memory_service::protocol::CONTEXTD_SOCKET_ENV) {
        return Some(std::path::PathBuf::from(p));
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join(".lunaris").join("codex-contextd.sock"))
}

/// Logs go to stderr so stdout stays a clean, pipeable payload. Quiet by
/// default: a CLI that narrates its own routing on every run is unusable in a
/// pipeline.
///
/// `lunaris try` is quieter still. At `warn` a first run emits three lines a
/// newcomer can do nothing about — Moon's `--maxmemory` auto-cap, a missing
/// `tokenizer.json` for the BPE token counter, and a `Scope::dev()` migration
/// notice from deep in the recall path. All three are true and none of them
/// belong in the first thirty seconds; every one reads as "something is wrong"
/// to someone who has no idea what a token counter is. `LUNARIS_CLI_LOG=warn`
/// brings them straight back, and the trial's own failures are printed by
/// `trial::run`, not by tracing, so nothing actionable is hidden.
fn init_tracing(trial: bool) {
    let default = if trial { "error" } else { "warn" };
    let filter = std::env::var("LUNARIS_CLI_LOG").unwrap_or_else(|_| default.to_owned());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .try_init();
}
