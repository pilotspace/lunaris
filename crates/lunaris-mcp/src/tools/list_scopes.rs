//! `memory.list_scopes` — enumerate known memory scopes.
//!
//! **Wave 2.C stub.** Full implementation lands in Wave 2.C:
//! - Read from `~/.lunaris/scopes.json` (the same registry `scope_resolver`
//!   maintains) or from the storage backend.
//! - Return name, creation timestamp, and derivation source for each scope.
//!
//! For now every call returns `ToolError::NotImplemented`.

use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.list_scopes`.
///
/// Currently empty — included as a struct so the MCP schema is stable
/// even if parameters are added in a later wave.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListScopesParams {}

/// Metadata for one memory scope.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScopeEntry {
    /// Canonical scope name (validated `[A-Za-z0-9_\-.]{1,128}`).
    pub name: String,
    /// ISO-8601 UTC creation timestamp.
    pub created_at: String,
    /// How the scope was derived: "override", "git-remote", "cwd-hash".
    pub source: String,
}

/// Output of a successful `memory.list_scopes` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ListScopesResponse {
    /// All known scopes, ordered alphabetically by name.
    pub scopes: Vec<ScopeEntry>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.list_scopes` (Wave 2.C stub — always returns `NotImplemented`).
#[allow(unused_variables)]
pub(crate) async fn handle(
    state: &AppState,
    params: ListScopesParams,
) -> Result<ListScopesResponse, ToolError> {
    Err(ToolError::NotImplemented)
}
