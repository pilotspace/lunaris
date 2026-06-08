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
//! | `memory.recall`           | 2.B  | implemented      |
//! | `memory.forget`           | 2.C  | implemented      |
//! | `memory.list_scopes`      | 2.C  | implemented      |
//! | `memory.record_decision`  | 25   | implemented      |
//! | `memory.record_edit`      | 25   | implemented      |
//! | `memory.status`           | 26   | implemented      |

pub(crate) mod forget;
pub(crate) mod ingest;
pub(crate) mod list_scopes;
pub(crate) mod recall;
pub(crate) mod record_decision;
pub(crate) mod record_edit;
pub(crate) mod staging;
pub(crate) mod status;

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
