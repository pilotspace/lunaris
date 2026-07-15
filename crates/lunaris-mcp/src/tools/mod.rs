//! MCP tool handlers for the Lunaris memory engine.
//!
//! Each submodule owns one tool's `Params`, `Response`, and `handle` function.
//! The `LunarisMcpServer` in `main.rs` delegates to these from its `#[tool]`
//! methods — keeping the method bodies thin and the business logic testable.
//!
//! ## Tool surface
//!
//! The six ENGINE-op handlers (`ingest`, `recall`, `forget`, `record_decision`,
//! `record_edit`, `status`) were lifted to the transport-neutral
//! `lunaris-memory-service` crate so `lunaris-contextd` shares one definition.
//! Only the session/registry-coupled handlers remain local here:
//!
//! | Tool                           | Wave | Status           |
//! |--------------------------------|------|------------------|
//! | `memory.list_scopes`           | 2.C  | local (registry) |
//! | `memory.scratchpad_write`      | qqb  | local (session)  |
//! | `memory.scratchpad_read`       | qqb  | local (session)  |
//! | `memory.scratchpad_grep`       | qqb  | local (session)  |
//! | `memory.scratchpad_consolidate`| dvi  | local (session)  |

pub(crate) mod list_scopes;
pub(crate) mod scratchpad_consolidate;
pub(crate) mod scratchpad_grep;
pub(crate) mod scratchpad_read;
pub(crate) mod scratchpad_write;
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
