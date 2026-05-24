//! `lunaris-mcp` — MCP server exposing Lunaris memory to Claude Code / Codex.
//!
//! Wave 0 scaffold: an rmcp stdio server with **zero tools registered**.
//! Wave 2 adds `memory.ingest`, `memory.recall`, `memory.forget`,
//! `memory.list_scopes`. Wave 1 adds scope resolution + lazy model staging.
//!
//! Transport: stdio only (MCP over JSON-RPC framed by Content-Length headers).
//! Auth: none — stdio is process-bound by the MCP client (Claude Code / Codex).
//!
//! # CRITICAL: logs go to stderr
//! stdout is the MCP transport. Writing anything to stdout corrupts the
//! JSON-RPC framing and causes the client to silently disconnect. Every
//! tracing subscriber writer in this crate MUST use `std::io::stderr`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

mod scope_resolver;

use clap::Parser;
use rmcp::{
    ServerHandler, ServiceExt,
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    transport::stdio,
};

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "lunaris-mcp",
    version,
    about = "Lunaris MCP server (stdio) — agent memory for Claude Code & Codex"
)]
struct Cli {
    /// Scope override (default: derived from git remote + branch, or cwd hash).
    /// Wave 1.A implements the full resolver; Wave 0 ignores this field.
    #[arg(long, env = "LUNARIS_MCP_SCOPE")]
    scope: Option<String>,

    /// Storage URL (default: sqlite:///~/.lunaris/<scope>.db).
    /// Wave 2 wires this to `Lunaris::open`; Wave 0 ignores it.
    #[arg(long, env = "LUNARIS_MCP_STORAGE")]
    storage: Option<String>,

    /// Tracing filter directive (RUST_LOG syntax).
    /// Logs always go to stderr — stdout is the MCP transport.
    #[arg(long, env = "LUNARIS_MCP_LOG", default_value = "info,rmcp=warn")]
    log_level: String,
}

// ── Server handler ────────────────────────────────────────────────────────────

/// Wave 0 MCP server — zero tools registered.
///
/// Responds to `initialize` (returns server capabilities + metadata) and
/// `tools/list` (returns an empty list). Wave 2 registers the four memory
/// tools via `#[tool_router]` / `#[tool_handler]`.
#[derive(Clone)]
struct LunarisMcpServer;

impl ServerHandler for LunarisMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                // Tools capability declared now; Wave 2 populates it.
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "Lunaris memory engine — ingest, recall, forget, and list memory scopes. \
             Wave 0 scaffold: tool handlers land in Wave 2."
                .to_string(),
        )
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        scope   = ?cli.scope,
        storage = ?cli.storage,
        "lunaris-mcp starting (Wave 0 scaffold — zero tools)",
    );

    run_server().await
}

/// Initialise `tracing_subscriber` with the given filter.
///
/// Writer is always `std::io::stderr` — stdout belongs to the MCP transport.
/// `try_init` is used so tests that install their own subscriber don't panic.
fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // NEVER write to stdout — it is the MCP JSON-RPC transport.
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Start the rmcp stdio server.
///
/// `stdio()` hooks `tokio::io::stdin()` / `tokio::io::stdout()` through the
/// MCP Content-Length framing codec. `.serve()` performs the MCP `initialize`
/// handshake; `.waiting()` drives the event loop until stdin EOF or SIGTERM.
///
/// Wave 2 will pass a constructed `Lunaris` handle into `LunarisMcpServer`
/// so tool handlers can call `Lunaris::ingest` / `retrieve` / `forget`.
async fn run_server() -> anyhow::Result<()> {
    let service = LunarisMcpServer
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(err = ?e, "MCP initialize handshake failed"))?;

    tracing::info!("MCP initialize handshake complete; entering request loop");

    service
        .waiting()
        .await
        .inspect_err(|e| tracing::error!(err = ?e, "MCP server exited with error"))?;

    tracing::info!("lunaris-mcp shut down cleanly");
    Ok(())
}
