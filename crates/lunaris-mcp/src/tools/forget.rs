//! `memory.forget` — delete memories by source prefix or episode ID.
//!
//! **Wave 2.C stub.** Full implementation lands in Wave 2.C:
//! - Accept a `target` discriminant (source_prefix OR episode_id).
//! - Delegate to `ScopedLunaris::forget` with the appropriate `ForgetTarget`.
//! - Return the count of removed episodes.
//!
//! For now every call returns `ToolError::NotImplemented`.

use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Discriminant specifying what to forget.
///
/// Exactly one field should be set; if both are set the handler
/// (Wave 2.C) will return `InvalidInput`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForgetTarget {
    /// Forget all episodes whose source starts with this prefix.
    #[serde(default)]
    pub source_prefix: Option<String>,

    /// Forget a single episode by its ULID string.
    #[serde(default)]
    pub episode_id: Option<String>,
}

/// Input parameters for `memory.forget`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForgetParams {
    /// What to delete.
    pub target: ForgetTarget,
}

/// Output of a successful `memory.forget` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ForgetResponse {
    /// Number of episodes logically removed.
    pub removed: u64,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.forget` (Wave 2.C stub — always returns `NotImplemented`).
#[allow(unused_variables)]
pub(crate) async fn handle(
    state: &AppState,
    params: ForgetParams,
) -> Result<ForgetResponse, ToolError> {
    Err(ToolError::NotImplemented)
}
