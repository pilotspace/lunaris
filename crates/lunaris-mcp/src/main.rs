//! `lunaris-mcp` — MCP server exposing Lunaris memory to Claude Code / Codex.
//!
//! MCP tools: `memory.ingest`, `memory.recall`, `memory.forget`,
//! `memory.list_scopes`, `memory.record_decision`, `memory.record_edit`, and
//! `memory.status`.
//!
//! Transport: stdio only (newline-delimited JSON-RPC — NDJSON, one object per line).
//! Auth: none — stdio is process-bound by the MCP client (Claude Code / Codex).
//!
//! # CRITICAL: logs go to stderr
//! stdout is the MCP transport. Writing anything to stdout corrupts the
//! JSON-RPC framing and causes the client to silently disconnect. Every
//! tracing subscriber writer in this crate MUST use `std::io::stderr`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

mod scope_resolver;
// Lazy GGUF model stager — Wave 1.B.
// NEVER called at server start: first call comes from the memory.recall handler
// (Wave 2.B). Cold-start budget gate (Wave 3.2) asserts tools/list < 500 ms.
mod model_stager;
mod state;
mod tools;

use clap::Parser;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};

use crate::state::AppState;
use crate::tools::{
    forget::{ForgetParams, ForgetResponse},
    ingest::{IngestParams, IngestResponse},
    list_scopes::{ListScopesParams, ListScopesResponse},
    recall::{RecallParams, RecallResponse},
    record_decision::{RecordDecisionParams, RecordDecisionResponse},
    record_edit::{RecordEditParams, RecordEditResponse},
    status::{StatusParams, StatusResponse},
};

// Alias for the JSON wrapper so tool method signatures stay concise.
use rmcp::handler::server::wrapper::Json;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "lunaris-mcp",
    version,
    about = "Lunaris MCP server (stdio) — agent memory for Claude Code & Codex"
)]
struct Cli {
    /// Scope override (default: derived from git remote + branch, or cwd hash).
    #[arg(long, env = "LUNARIS_MCP_SCOPE")]
    scope: Option<String>,

    /// Storage URL (default: sqlite:///<HOME>/.lunaris/<scope>.db).
    #[arg(long, env = "LUNARIS_MCP_STORAGE")]
    storage: Option<String>,

    /// Tracing filter directive (RUST_LOG syntax).
    /// Logs always go to stderr — stdout is the MCP transport.
    #[arg(long, env = "LUNARIS_MCP_LOG", default_value = "info,rmcp=warn")]
    log_level: String,
}

// ── Server handler ────────────────────────────────────────────────────────────

/// Lunaris MCP server — memory tools registered.
///
/// `state` is `Clone`-able because `AppState` wraps `Arc<Lunaris>`. The
/// `ToolRouter` is constructed once at startup via `Self::lunaris_tool_router()`
/// and stored alongside state so `#[tool_handler]` can dispatch calls.
#[derive(Clone)]
struct LunarisMcpServer {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl LunarisMcpServer {
    fn new(state: AppState) -> Self {
        Self { state, tool_router: Self::lunaris_tool_router() }
    }
}

// ── Tool registration ─────────────────────────────────────────────────────────
//
// `#[tool_router]` generates `Self::lunaris_tool_router() -> ToolRouter<Self>`
// from the annotated methods. `#[tool_handler(router = self.tool_router)]` on
// the `impl ServerHandler` block wires `call_tool` + `list_tools` dispatch
// to that router, while leaving `get_info` free for our custom implementation.

#[tool_router(router = lunaris_tool_router)]
impl LunarisMcpServer {
    /// Ingest an observation into agent memory.
    ///
    /// Stores the observation as an episode scoped to this server's active
    /// scope. Returns the log-sequence number of the committed write.
    #[tool(
        name = "memory.ingest",
        description = "Ingest an observation (message, document, tool result) into agent memory. \
                       Returns the LSN of the committed write."
    )]
    async fn ingest(
        &self,
        Parameters(params): Parameters<IngestParams>,
    ) -> Result<Json<IngestResponse>, rmcp::ErrorData> {
        tools::ingest::handle(&self.state, params).await.map(Json).map_err(rmcp::ErrorData::from)
    }

    /// Recall memories relevant to a query.
    ///
    /// Fuses semantic (vector) and keyword (BM25) search via Reciprocal Rank
    /// Fusion. Wave 2.B implements the full retrieval path.
    #[tool(
        name = "memory.recall",
        description = "Recall memories relevant to a natural-language query using \
                       hybrid semantic + BM25 retrieval with optional source-prefix and \
                       as_of filters."
    )]
    async fn recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
    ) -> Result<Json<RecallResponse>, rmcp::ErrorData> {
        tools::recall::handle(&self.state, params).await.map(Json).map_err(rmcp::ErrorData::from)
    }

    /// Forget memories by source prefix or episode ID.
    ///
    /// Wave 2.C implements the full deletion path.
    #[tool(name = "memory.forget", description = "Delete memories by source prefix or episode ID.")]
    async fn forget(
        &self,
        Parameters(params): Parameters<ForgetParams>,
    ) -> Result<Json<ForgetResponse>, rmcp::ErrorData> {
        tools::forget::handle(&self.state, params).await.map(Json).map_err(rmcp::ErrorData::from)
    }

    /// List all known memory scopes.
    ///
    /// Wave 2.C implements the full scope enumeration path.
    #[tool(
        name = "memory.list_scopes",
        description = "List all known memory scopes with creation timestamps."
    )]
    async fn list_scopes(
        &self,
        Parameters(params): Parameters<ListScopesParams>,
    ) -> Result<Json<ListScopesResponse>, rmcp::ErrorData> {
        tools::list_scopes::handle(&self.state, params)
            .await
            .map(Json)
            .map_err(rmcp::ErrorData::from)
    }

    /// Record an architectural or scoping decision into agent memory.
    ///
    /// Writes an Episode with `source = "decision:<scope>"` and typed metadata
    /// (`kind = "decision"`, `tag_count = N`). The content is a JSON serialisation
    /// of the structured payload (decision, rationale, alternatives, tags).
    /// Accepts an optional `dedupe_key` for replay-safe idempotency (HOOK-05 path).
    #[tool(
        name = "memory.record_decision",
        description = "Record an architectural or scoping decision into agent memory. \
                       Writes an Episode with source='decision:<scope>' and metadata \
                       kind='decision'. Accepts optional dedupe_key for replay-safe \
                       idempotency."
    )]
    async fn record_decision(
        &self,
        Parameters(params): Parameters<RecordDecisionParams>,
    ) -> Result<Json<RecordDecisionResponse>, rmcp::ErrorData> {
        tools::record_decision::handle(&self.state, params)
            .await
            .map(Json)
            .map_err(rmcp::ErrorData::from)
    }

    /// Record a file edit into agent memory.
    ///
    /// Writes an Episode with `source = "edit:<scope>"` and metadata
    /// `{"kind": "edit", "path": "<value>"}`. The `path` field in metadata
    /// enables future `memory.recall` filter-by-path queries.
    /// Accepts optional `dedupe_key` for replay-safe idempotency (HOOK-05).
    #[tool(
        name = "memory.record_edit",
        description = "Record a file edit into agent memory. \
                       Writes an Episode with source='edit:<scope>'. \
                       The path field is stored in metadata for future path-filter recalls. \
                       Accepts optional dedupe_key for replay-safe idempotency."
    )]
    async fn record_edit(
        &self,
        Parameters(params): Parameters<RecordEditParams>,
    ) -> Result<Json<RecordEditResponse>, rmcp::ErrorData> {
        tools::record_edit::handle(&self.state, params)
            .await
            .map(Json)
            .map_err(rmcp::ErrorData::from)
    }

    /// Report backend capabilities and queue health for the bound scope.
    #[tool(
        name = "memory.status",
        description = "Report Lunaris backend capabilities and MQ-backed queue health \
                       for the active memory scope."
    )]
    async fn status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<Json<StatusResponse>, rmcp::ErrorData> {
        tools::status::handle(&self.state, params).await.map(Json).map_err(rmcp::ErrorData::from)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LunarisMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Lunaris memory engine — ingest, recall, forget, list memory scopes, \
             record decisions, record edits, and inspect backend status. Scope is bound at server startup."
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
        "lunaris-mcp starting",
    );

    run_server(&cli).await
}

/// Initialise `tracing_subscriber` with the given filter.
///
/// Writer is always `std::io::stderr` — stdout belongs to the MCP transport.
/// `try_init` is used so tests that install their own subscriber don't panic.
fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info")))
        // NEVER write to stdout — it is the MCP JSON-RPC transport.
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Bootstrap `AppState` and start the rmcp stdio server.
///
/// `stdio()` hooks `tokio::io::stdin()` / `tokio::io::stdout()` through the
/// MCP Content-Length framing codec. `.serve()` performs the MCP `initialize`
/// handshake; `.waiting()` drives the event loop until stdin EOF or SIGTERM.
async fn run_server(cli: &Cli) -> anyhow::Result<()> {
    let state = AppState::bootstrap(cli.scope.as_deref(), cli.storage.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("bootstrap failed: {e}"))?;

    tracing::info!(scope = state.scope.as_str(), "lunaris-mcp ready");

    let service = LunarisMcpServer::new(state)
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
