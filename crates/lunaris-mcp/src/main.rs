//! `lunaris-mcp` — MCP server exposing Lunaris memory to Claude Code / Codex.
//!
//! MCP tools: `memory.ingest`, `memory.recall`, `memory.forget`,
//! `memory.list_scopes`, `memory.record_decision`, `memory.record_edit`,
//! `memory.status`, `memory.scratchpad_write`, `memory.scratchpad_read`,
//! `memory.scratchpad_grep`, and `memory.scratchpad_consolidate`.
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
// In-process Moon server lifecycle — all items cfg(feature="embedded-moon").
// Compiled to nothing on the default (no feature) build.
mod embedded_moon;
// Per-session scratchpad resolution + handover tracking — reads the
// sessions.json marker lunaris-hook maintains (scratchpad-handover task).
mod session_pad;
// Socket-first proxy to lunaris-contextd with direct-open fallback
// (contextd-mcp-merge + scratchpad-proxiable). Routes the 6 engine ops AND the
// 4 scratchpad ops (+ session handover) to the warm daemon; on transport
// failure the circuit-breaker flips to the identical shared handler.
mod proxy;
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
// Engine-op DTOs now live in the transport-neutral shared crate
// (lunaris-memory-service), called by BOTH this mcp server and contextd.
use lunaris_memory_service::protocol::MemoryRequest;
use lunaris_memory_service::{
    forget::{ForgetParams, ForgetResponse},
    ingest::{IngestParams, IngestResponse},
    recall::{RecallParams, RecallResponse},
    record_decision::{RecordDecisionParams, RecordDecisionResponse},
    record_edit::{RecordEditParams, RecordEditResponse},
    scratchpad_consolidate::{ScratchpadConsolidateParams, ScratchpadConsolidateResponse},
    scratchpad_grep::{ScratchpadGrepParams, ScratchpadGrepResponse},
    scratchpad_read::{ScratchpadReadParams, ScratchpadReadResponse},
    scratchpad_write::{ScratchpadWriteParams, ScratchpadWriteResponse},
    status::{StatusParams, StatusResponse},
};
// list_scopes stays local to mcp (registry coupling). Scratchpad ops now proxy
// like the engine ops, but their session-aware namespace is resolved mcp-side.
use crate::tools::list_scopes::{ListScopesParams, ListScopesResponse};

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
    /// Socket-first proxy to lunaris-contextd with direct-open fallback. Shared
    /// (`Arc`) so the per-session route/breaker state is one instance across the
    /// cloned handler the rmcp router hands to each request.
    proxy: std::sync::Arc<proxy::MemoryProxy>,
    tool_router: ToolRouter<Self>,
}

impl LunarisMcpServer {
    fn new(state: AppState) -> Self {
        Self {
            state,
            proxy: std::sync::Arc::new(proxy::MemoryProxy::new()),
            tool_router: Self::lunaris_tool_router(),
        }
    }
}

/// Map a shared-service error into an rmcp wire error.
///
/// The orphan rule forbids `impl From<ServiceError> for rmcp::ErrorData`
/// (both types are foreign to this crate), so the conversion lives here as a
/// free function. Mirrors the local `ToolError -> ErrorData` mapping:
/// engine failures are internal errors; input-validation failures are
/// invalid-params.
fn map_service_error(e: lunaris_memory_service::ServiceError) -> rmcp::ErrorData {
    use lunaris_memory_service::ServiceError;
    match e {
        ServiceError::LunarisEngine(inner) => {
            rmcp::ErrorData::internal_error(inner.to_string(), None)
        }
        ServiceError::InvalidInput(msg) => rmcp::ErrorData::invalid_params(msg, None),
    }
}

/// Decode the proxy's JSON `data` back into a tool's rmcp response DTO. The
/// socket path returns the daemon's DTO as `serde_json::Value`; the direct
/// path serializes its own DTO to the same shape — so both routes re-hydrate
/// through here into the exact `Json<R>` the `#[tool]` method promises.
fn decode_dto<R: serde::de::DeserializeOwned>(
    data: serde_json::Value,
) -> Result<Json<R>, rmcp::ErrorData> {
    serde_json::from_value(data)
        .map(Json)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("proxy DTO decode failed: {e}"), None))
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
        description = "Store an observation (message, document, tool result) as a durable episode \
                       in agent memory. Returns the LSN of the committed write. Use for facts, \
                       summaries, and any content that must survive beyond the current session. \
                       Writes are fast — no embedder load on ingest. \
                       Tip: for structured provenance use memory.record_decision or memory.record_edit instead."
    )]
    async fn ingest(
        &self,
        Parameters(params): Parameters<IngestParams>,
    ) -> Result<Json<IngestResponse>, rmcp::ErrorData> {
        let req = MemoryRequest::Ingest { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Recall memories relevant to a query.
    ///
    /// Fuses semantic (vector) and keyword (BM25) search via Reciprocal Rank
    /// Fusion. Wave 2.B implements the full retrieval path.
    #[tool(
        name = "memory.recall",
        description = "Hybrid semantic + BM25 recall over durable memory. Returns up to k hits \
                       (default k=5) as curated LLM-ready snippets (≤260 chars) — stored JSON envelopes \
                       render as 'decision:'/'edit'/'tool output:'/'prompt:' summaries, NOT full episode text. \
                       Pass raw=true for the uncurated stored bytes (200-char preview) when debugging. \
                       Each hit includes episode_id, source, content, score, and ingested_at. \
                       IMPORTANT: first call in a session triggers GGUF embedder load (slow); subsequent calls are fast. \
                       Progressive-disclosure strategy: start with k=5 (cheap preview pass); if no useful hit found, \
                       widen: raise k, add filters.source_prefix ('decision:', 'edit:', 'lunaris:'), or set as_of \
                       for a bi-temporal point-in-time snapshot. as_of requires a bi-temporal backend (SQLite yes, \
                       Moon not yet) — on a non-bi-temporal backend the call errors instead of silently returning \
                       current-state data. SQLite backend falls back to vector-only (no BM25)."
    )]
    async fn recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
    ) -> Result<Json<RecallResponse>, rmcp::ErrorData> {
        // Staging is deferred to the proxy's DIRECT path only: when the socket
        // path serves the recall, contextd's warm handle already staged the
        // embedder, so the mcp server must not download the GGUF needlessly.
        let req = MemoryRequest::Recall { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Forget memories by source prefix or episode ID.
    ///
    /// Wave 2.C implements the full deletion path.
    #[tool(
        name = "memory.forget",
        description = "Delete durable memory episodes by source prefix or episode ID. \
                       Use source_prefix to bulk-delete (e.g. 'edit:' removes all edit-provenance records). \
                       Use episode_id for surgical removal of a single episode. Irreversible."
    )]
    async fn forget(
        &self,
        Parameters(params): Parameters<ForgetParams>,
    ) -> Result<Json<ForgetResponse>, rmcp::ErrorData> {
        let req = MemoryRequest::Forget { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// List all known memory scopes.
    ///
    /// Wave 2.C implements the full scope enumeration path.
    #[tool(
        name = "memory.list_scopes",
        description = "List all known memory scopes with creation timestamps. Diagnostic tool — use \
                       to confirm the active scope or enumerate multi-scope deployments. Not for content retrieval."
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
        description = "Record an architectural or scoping decision as a durable, structured episode \
                       with source='decision:<scope>'. The source prefix enables targeted retrieval: pass \
                       filters.source_prefix='decision:' to memory.recall to surface only decision records. \
                       Accepts optional dedupe_key for replay-safe idempotency (hook pipeline integration; \
                       enforced on SQLite and Moon — Postgres falls through to a fresh write). \
                       Stores content as JSON with decision, rationale, alternatives, and tags fields."
    )]
    async fn record_decision(
        &self,
        Parameters(params): Parameters<RecordDecisionParams>,
    ) -> Result<Json<RecordDecisionResponse>, rmcp::ErrorData> {
        let req =
            MemoryRequest::RecordDecision { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Record a file edit into agent memory.
    ///
    /// Writes an Episode with `source = "edit:<scope>"` and metadata
    /// `{"kind": "edit", "path": "<value>"}`. The `path` field in metadata
    /// enables future `memory.recall` filter-by-path queries.
    /// Accepts optional `dedupe_key` for replay-safe idempotency (HOOK-05).
    #[tool(
        name = "memory.record_edit",
        description = "Record a file edit as a durable episode with source='edit:<scope>' and metadata path field. \
                       The source prefix enables targeted retrieval: pass filters.source_prefix='edit:' to \
                       memory.recall to surface only edit records. The path metadata field enables future \
                       path-filter queries (filters.source_prefix='edit:' combined with k widen). \
                       Accepts optional dedupe_key for replay-safe idempotency (enforced on SQLite and Moon — \
                       Postgres falls through to a fresh write)."
    )]
    async fn record_edit(
        &self,
        Parameters(params): Parameters<RecordEditParams>,
    ) -> Result<Json<RecordEditResponse>, rmcp::ErrorData> {
        let req = MemoryRequest::RecordEdit { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Report backend capabilities and queue health for the bound scope.
    #[tool(
        name = "memory.status",
        description = "Report backend capabilities (queue_native, graph_native, vector_native) and MQ queue health \
                       for the active scope. Diagnostic tool — use to verify the backend before calling \
                       memory.scratchpad_consolidate (requires queue_native=true) or to confirm vector support \
                       before a semantic recall pass."
    )]
    async fn status(
        &self,
        Parameters(_params): Parameters<StatusParams>,
    ) -> Result<Json<StatusResponse>, rmcp::ErrorData> {
        let req = MemoryRequest::Status { scope: self.state.scope.as_str().to_owned() };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Write a key-value pair to the agent scratchpad.
    ///
    /// Keys are namespaced under `{namespace}{key}` on the Episode `source` field.
    /// The `namespace` defaults to "scratchpad/" and is NOT a security boundary —
    /// scope isolation is bound at server startup.
    #[tool(
        name = "memory.scratchpad_write",
        description = "Write a key-value pair to the ephemeral agent scratchpad. Returns the LSN of the write. \
                       Fast — no embedder load. Keys are namespaced: full source key = '{namespace}{key}' \
                       (namespace defaults to 'scratchpad/'). ':' is rejected in namespace. \
                       Scratchpad is NOT semantically indexed — use memory.ingest for durable, queryable memory. \
                       Write-then-read is read-your-writes consistent on all backends."
    )]
    async fn scratchpad_write(
        &self,
        Parameters(mut params): Parameters<ScratchpadWriteParams>,
    ) -> Result<Json<ScratchpadWriteResponse>, rmcp::ErrorData> {
        // Resolve the session-aware namespace mcp-side (reads the sessions.json
        // marker; may fire a handover THROUGH the proxy), then route the write
        // through the proxy so it lands on contextd's warm engine.
        let ns = tools::staging::resolve_namespace_session_aware(
            &self.proxy,
            &self.state,
            params.namespace,
        )
        .await
        .map_err(rmcp::ErrorData::from)?;
        params.namespace = Some(ns);
        let req =
            MemoryRequest::ScratchpadWrite { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Read a single key from the agent scratchpad.
    ///
    /// Returns the verbatim stored value (recovered from the Episode `content`,
    /// not the chunked text). Write-then-read is read-your-writes consistent on
    /// every backend — Moon indexes synchronously inline in HSET, and the sqlite
    /// default enforces the `source` filter at the SQL boundary.
    #[tool(
        name = "memory.scratchpad_read",
        description = "Exact key lookup on the agent scratchpad. Returns {found, value?} where value is the \
                       FULL verbatim stored JSON — not a 200-char snippet. No model load. \
                       Use as the FIRST retrieval tier when you know the exact key (cheaper than memory.recall). \
                       Namespace defaults to 'scratchpad/'. Read-your-writes consistent on all backends."
    )]
    async fn scratchpad_read(
        &self,
        Parameters(mut params): Parameters<ScratchpadReadParams>,
    ) -> Result<Json<ScratchpadReadResponse>, rmcp::ErrorData> {
        let ns = tools::staging::resolve_namespace_session_aware(
            &self.proxy,
            &self.state,
            params.namespace,
        )
        .await
        .map_err(rmcp::ErrorData::from)?;
        params.namespace = Some(ns);
        let req =
            MemoryRequest::ScratchpadRead { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// Grep scratchpad entries by key prefix pattern.
    #[tool(
        name = "memory.scratchpad_grep",
        description = "Prefix listing over the agent scratchpad. Returns all entries whose source key starts with \
                       '{namespace}{pattern}', each with the full verbatim stored value. No model load. \
                       Use as the FIRST retrieval tier when you know a key prefix but not the exact key. \
                       Namespace defaults to 'scratchpad/'. Cheaper than memory.recall; returns full values not previews."
    )]
    async fn scratchpad_grep(
        &self,
        Parameters(mut params): Parameters<ScratchpadGrepParams>,
    ) -> Result<Json<ScratchpadGrepResponse>, rmcp::ErrorData> {
        let ns = tools::staging::resolve_namespace_session_aware(
            &self.proxy,
            &self.state,
            params.namespace,
        )
        .await
        .map_err(rmcp::ErrorData::from)?;
        params.namespace = Some(ns);
        let req =
            MemoryRequest::ScratchpadGrep { scope: self.state.scope.as_str().to_owned(), params };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }

    /// On-demand ACT-R consolidation drain for the scratchpad.
    ///
    /// Requires a Moon backend (queue_native=true). Returns promotions and archives
    /// counts from the ConsolidationReport. Guards: fails if the background
    /// consolidation worker is live (WorkerConflict), backend has no native queue
    /// (UnsupportedBackend), or drain exceeds 5s hard timeout (Timeout).
    #[tool(
        name = "memory.scratchpad_consolidate",
        description = "On-demand ACT-R consolidation drain. Moves promoted scratchpad items to durable memory and \
                       archives low-activation items. Returns {promotions, archives} counts. \
                       Requires Moon backend (queue_native=true — check memory.status first). \
                       Guards: fails with WorkerConflict if background consolidation worker is live, \
                       UnsupportedBackend if backend lacks a native queue, or Timeout if drain exceeds 5s."
    )]
    async fn scratchpad_consolidate(
        &self,
        Parameters(params): Parameters<ScratchpadConsolidateParams>,
    ) -> Result<Json<ScratchpadConsolidateResponse>, rmcp::ErrorData> {
        // No session resolution: consolidate has explicit-namespace semantics.
        let req = MemoryRequest::ScratchpadConsolidate {
            scope: self.state.scope.as_str().to_owned(),
            params,
        };
        decode_dto(self.proxy.dispatch(&self.state, req).await?)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LunarisMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Lunaris progressive-disclosure memory — use tools in this order to minimize latency and model load:\n\
                 \n\
                 RETRIEVE (cheapest first):\n\
                 1. memory.scratchpad_read / memory.scratchpad_grep — exact or prefix key lookup; returns full verbatim \
                 values; no model load; always try this first for known keys.\n\
                 2. memory.recall k=5 (default) — semantic + BM25 hybrid preview pass; first call triggers GGUF \
                 embedder load (slow); returns curated LLM-ready snippets (≤260 chars; JSON envelopes summarized \
                 as 'decision:'/'edit'/'tool output:'/'prompt:') with episode_id and score. If the snippet \
                 is sufficient, stop here.\n\
                 3. Widen recall — raise k, add filters.source_prefix ('decision:', 'edit:', 'lunaris:'), or \
                 pass as_of for a point-in-time snapshot. Use this tier only when the k=5 pass returns no useful hit.\n\
                 \n\
                 STORE (choose by lifetime):\n\
                 - memory.ingest — durable observations, documents, tool results.\n\
                 - memory.record_decision / memory.record_edit — structured provenance; source prefix enables \
                 targeted recall later (filters.source_prefix='decision:' recalls only decisions).\n\
                 - memory.scratchpad_write — ephemeral working state (key-value); fast; not semantically indexed.\n\
                 \n\
                 DIAGNOSTICS (not for content retrieval):\n\
                 - memory.status — backend capabilities and queue health.\n\
                 - memory.list_scopes — enumerate known scopes.\n\
                 - memory.scratchpad_consolidate — on-demand ACT-R drain; Moon backend only.\n\
                 \n\
                 Note: recall returns curated preview snippets — not full episode content (raw=true gives \
                 the uncurated stored bytes, 200-char cap). There is no fetch-full-episode tool; widen k or \
                 use scratchpad_read for full verbatim values."
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

    // Capture the embedded-Moon guard before `state` is moved into
    // LunarisMcpServer::new. Arc::clone bumps the refcount cheaply; the
    // original guard inside `state` stays alive until the state move.
    #[cfg(feature = "embedded-moon")]
    let moon_guard = state._embedded_moon.clone();

    let service = LunarisMcpServer::new(state)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(err = ?e, "MCP initialize handshake failed"))?;

    tracing::info!("MCP initialize handshake complete; entering request loop");

    let wait_result = service.waiting().await;

    // Graceful embedded-Moon shutdown: flush AOF before the process exits.
    // Called on BOTH Ok (clean EOF/SIGTERM) and Err (MCP loop error) paths so
    // Moon's AOF is flushed in all cases. Drop alone aborts Moon synchronously
    // and can cut the flush mid-write.
    // Do NOT add a separate tokio::signal::ctrl_c select here —
    // .waiting() already handles SIGTERM.
    #[cfg(feature = "embedded-moon")]
    if let Some(guard) = moon_guard {
        tracing::info!("shutting down embedded Moon (AOF flush)");
        guard.shutdown().await;
    }

    wait_result.inspect_err(|e| tracing::error!(err = ?e, "MCP server exited with error"))?;

    tracing::info!("lunaris-mcp shut down cleanly");
    Ok(())
}
