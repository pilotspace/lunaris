//! MCP tool handlers for the Lunaris memory engine.
//!
//! Each submodule owns one tool's `Params`, `Response`, and `handle` function.
//! The `LunarisMcpServer` in `main.rs` delegates to these from its `#[tool]`
//! methods — keeping the method bodies thin and the business logic testable.
//!
//! ## Tool surface
//!
//! | Tool                      | Wave | Status           |
//! |---------------------------|------|------------------|
//! | `memory.ingest`           | 2.A  | implemented      |
//! | `memory.recall`           | 2.B  | stub (NotImpl)   |
//! | `memory.forget`           | 2.C  | stub (NotImpl)   |
//! | `memory.list_scopes`      | 2.C  | stub (NotImpl)   |
//! | `memory.record_decision`  | 25   | implemented      |

pub(crate) mod forget;
pub(crate) mod ingest;
pub(crate) mod list_scopes;
pub(crate) mod recall;
pub(crate) mod record_decision;

// ── Shared error type ─────────────────────────────────────────────────────────

use rmcp::ErrorData;
use thiserror::Error;

/// Tool-layer error, converted to [`rmcp::ErrorData`] at the `#[tool]` boundary.
///
/// `LunarisEngine` maps storage/ingest failures. `NotImplemented` is used for
/// Wave 2.B/2.C stubs. `InvalidInput` surfaces field-validation errors that
/// `serde` cannot catch (e.g. unparseable RFC-3339 strings).
#[derive(Debug, Error)]
pub(crate) enum ToolError {
    #[error("lunaris engine: {0}")]
    LunarisEngine(#[from] lunaris_core::LunarisError),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("not yet implemented in this wave")]
    NotImplemented,
}

impl From<ToolError> for ErrorData {
    fn from(e: ToolError) -> ErrorData {
        match e {
            ToolError::LunarisEngine(inner) => ErrorData::internal_error(inner.to_string(), None),
            ToolError::InvalidInput(msg) => ErrorData::invalid_params(msg, None),
            ToolError::NotImplemented => ErrorData::internal_error("not yet implemented", None),
        }
    }
}
