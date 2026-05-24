//! `memory.recall` — semantic + keyword retrieval over agent memory.
//!
//! **Wave 2.B stub.** Full implementation lands in Wave 2.B:
//! - Stage the GGUF embedder via `model_stager::ensure_staged`.
//! - Build a `RetrievalBuilder` with vector + keyword fused via RRF.
//! - Apply `as_of` bi-temporal filter when provided.
//!
//! For now every call returns `ToolError::NotImplemented`.

use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Optional filters for `memory.recall`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallFilters {
    /// Only return memories whose source starts with this prefix.
    #[serde(default)]
    pub source_prefix: Option<String>,
}

/// Input parameters for `memory.recall`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallParams {
    /// Natural-language query to recall memories for.
    pub query: String,

    /// Maximum number of hits to return (default: 5).
    #[serde(default = "default_k")]
    pub k: usize,

    /// Optional source/metadata filters.
    #[serde(default)]
    pub filters: Option<RecallFilters>,

    /// Bi-temporal as-of time in RFC-3339 (default: latest).
    #[serde(default)]
    pub as_of: Option<String>,
}

fn default_k() -> usize {
    5
}

/// A single recalled memory hit.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecallHit {
    /// Episode ID (ULID string).
    pub episode_id: String,
    /// Logical source of the episode.
    pub source: String,
    /// Text content of the recalled episode.
    pub content: String,
    /// Combined retrieval score ∈ [0, 1].
    pub score: f32,
    /// Ingest wall-clock timestamp in RFC-3339.
    pub ingested_at: String,
}

/// Output of a successful `memory.recall` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecallResponse {
    /// Ordered list of recalled memories (highest score first).
    pub hits: Vec<RecallHit>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.recall` (Wave 2.B stub — always returns `NotImplemented`).
///
/// Wave 2.B will:
/// 1. Call `model_stager::ensure_staged(ModelKind::Embedder)` to trigger lazy GGUF download.
/// 2. Build a `RetrievalBuilder` for the scoped Lunaris handle.
/// 3. Optionally apply `as_of` bi-temporal filter and source prefix filter.
/// 4. Rerank results and return up to `params.k` hits.
#[allow(unused_variables)]
pub(crate) async fn handle(
    state: &AppState,
    params: RecallParams,
) -> Result<RecallResponse, ToolError> {
    Err(ToolError::NotImplemented)
}
