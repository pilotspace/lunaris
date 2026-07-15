//! MCP tool handlers for the Lunaris memory engine.
//!
//! Each submodule owns one tool's `Params`, `Response`, and `handle` function.
//! The `LunarisMcpServer` in `main.rs` delegates to these from its `#[tool]`
//! methods — keeping the method bodies thin and the business logic testable.
//!
//! ## Tool surface
//!
//! The six ENGINE-op handlers (`ingest`, `recall`, `forget`, `record_decision`,
//! `record_edit`, `status`) AND the four scratchpad handlers were lifted to the
//! transport-neutral `lunaris-memory-service` crate so `lunaris-contextd` shares
//! one definition and the scratchpad ops proxy like the engine ops
//! (scratchpad-proxiable task). What remains local here:
//!
//! | Tool                           | Local because…                          |
//! |--------------------------------|-----------------------------------------|
//! | `memory.list_scopes`           | storage-registry coupling               |
//! | `staging`                      | model-stage seam + session-aware ns     |
//! | `scratchpad_consolidate` (mod) | Moon-backed integration tests only      |
//!
//! The scratchpad `#[tool]` methods in `main.rs` resolve the session-aware
//! namespace via `staging::resolve_namespace_session_aware` (reads the local
//! sessions.json marker; fires the handover THROUGH the proxy) and then route
//! the op through the proxy to contextd's warm engine.

pub(crate) mod list_scopes;
/// Moon-backed `scratchpad_consolidate` integration tests (test-only module; the
/// handler moved to `lunaris_memory_service::scratchpad_consolidate`).
pub(crate) mod scratchpad_consolidate;
pub(crate) mod staging;

// ── Shared error type ─────────────────────────────────────────────────────────

use rmcp::ErrorData;
use thiserror::Error;

/// Tool-layer error, converted to [`rmcp::ErrorData`] at the `#[tool]` boundary.
///
/// `LunarisEngine` maps storage/ingest failures. `InvalidInput` surfaces
/// field-validation errors that `serde` cannot catch (e.g. unparseable
/// RFC-3339 strings).
#[derive(Debug, Error)]
pub(crate) enum ToolError {
    #[error("lunaris engine: {0}")]
    LunarisEngine(#[from] lunaris_core::LunarisError),

    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl From<ToolError> for ErrorData {
    fn from(e: ToolError) -> ErrorData {
        match e {
            ToolError::LunarisEngine(inner) => ErrorData::internal_error(inner.to_string(), None),
            ToolError::InvalidInput(msg) => ErrorData::invalid_params(msg, None),
        }
    }
}
