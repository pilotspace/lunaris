//! Transport-neutral Lunaris memory-engine tool handlers.
//!
//! Each submodule owns one tool's `Params`, `Response`, and an
//! `async fn handle(lunaris: &Lunaris, scope: &Scope, params) -> Result<_, ServiceError>`.
//! The handlers are SINGLE-SOURCE: `lunaris-mcp` calls them from its `#[tool]`
//! methods (proxy + direct-open fallback) and `lunaris-contextd` calls them
//! from its warm-engine socket dispatch. Neither holds a private copy.
//!
//! Deliberately transport-neutral: no `rmcp` (the `ServiceError -> rmcp::ErrorData`
//! conversion lives at the mcp boundary) and no model staging (a CALLER concern —
//! the mcp `#[tool]` wrapper stages lazily; contextd stages at warm-up).

use lunaris_core::{LunarisError, StorageError};
use thiserror::Error;

// In-process Moon server launcher — ONE definition shared by lunaris-mcp and
// lunaris-contextd. Feature-gated: `embedded-moon` is NEVER in default
// (CLAUDE.md invariant — workspace builds must not compile the moon server).
#[cfg(feature = "embedded-moon")]
pub mod embedded_moon;

pub mod feedback;
pub mod forget;
pub mod handover;
pub mod ingest;
pub mod namespace;
pub mod protocol;
pub mod recall;
pub mod record_decision;
pub mod record_edit;
pub mod scratchpad_consolidate;
pub mod scratchpad_grep;
pub mod scratchpad_read;
pub mod scratchpad_write;
pub mod status;

/// Tool-layer error. The `lunaris-mcp` boundary converts this into
/// `rmcp::ErrorData`; `lunaris-contextd` maps it to a `MemoryResponse::Err`.
///
/// `LunarisEngine` carries storage/ingest failures. `InvalidInput` surfaces
/// field-validation errors serde cannot catch (e.g. unparseable RFC-3339).
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("lunaris engine: {0}")]
    LunarisEngine(#[from] LunarisError),

    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// Return `true` when `err` is a `StorageError::NotSupported` from the keyword
/// branch (e.g. the embedded/SQLite backend lacking FTS5 BM25). recall,
/// scratchpad_read, and scratchpad_grep catch this and retry vector-only.
/// Relocated verbatim from the mcp `staging` module — a generic error
/// classifier, not a staging concern.
pub fn is_keyword_not_supported(err: &LunarisError) -> bool {
    matches!(
        err,
        LunarisError::Storage(StorageError::NotSupported(msg))
            if msg.contains("keyword_search") || msg.contains("keyword")
    )
}
