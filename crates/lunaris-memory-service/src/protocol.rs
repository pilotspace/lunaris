//! Wire protocol shared by the two client surfaces (contextd-mcp-merge).
//!
//! `MemoryRequest` / `MemoryResponse` are the socket contract between the thin
//! `lunaris-mcp` proxy and the warm `lunaris-contextd` daemon. They live HERE
//! (the transport-neutral shared crate), not in either client, so neither peer
//! depends on the other — and [`dispatch`] is the SINGLE variant→handler map
//! both the contextd socket path and the mcp direct-open fallback call, so the
//! two cannot diverge.
//!
//! Framing is the caller's concern (one JSON request/response, connection-per-
//! call); this module only defines the shapes and the pure dispatch.

use lunaris::Lunaris;
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ServiceError;

/// Default contextd socket, mirrored by both peers. Override with
/// `LUNARIS_CONTEXTD_SOCKET`. Kept here so the mcp proxy and the daemon agree
/// on one path without either depending on the other's crate.
pub const CONTEXTD_SOCKET_ENV: &str = "LUNARIS_CONTEXTD_SOCKET";

/// Engine-op request mirroring the stateless `memory.*` MCP tools.
///
/// Only the SIX engine ops cross the socket — the session/registry-coupled
/// tools (`scratchpad_*`, `list_scopes`) are served locally by the mcp server
/// and never proxied. Each variant carries an explicit `scope` (trusted
/// local-peer model, §3 FROZEN @ v1: the 0700 user socket is the trust
/// boundary) plus the tool's own wire DTO as `params`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MemoryRequest {
    Ingest { scope: String, params: crate::ingest::IngestParams },
    Recall { scope: String, params: crate::recall::RecallParams },
    Forget { scope: String, params: crate::forget::ForgetParams },
    RecordDecision { scope: String, params: crate::record_decision::RecordDecisionParams },
    RecordEdit { scope: String, params: crate::record_edit::RecordEditParams },
    Status { scope: String },
}

impl MemoryRequest {
    /// Borrow the peer-supplied scope string. Every variant carries `scope`, so
    /// this never returns empty by construction — but an empty *string* is
    /// still rejected by `Scope::new` downstream (a `scope_required` fault).
    pub fn scope(&self) -> &str {
        match self {
            MemoryRequest::Ingest { scope, .. }
            | MemoryRequest::Recall { scope, .. }
            | MemoryRequest::Forget { scope, .. }
            | MemoryRequest::RecordDecision { scope, .. }
            | MemoryRequest::RecordEdit { scope, .. }
            | MemoryRequest::Status { scope, .. } => scope,
        }
    }

    /// A short op label for logs/metrics (no scope or payload).
    pub fn op(&self) -> &'static str {
        match self {
            MemoryRequest::Ingest { .. } => "ingest",
            MemoryRequest::Recall { .. } => "recall",
            MemoryRequest::Forget { .. } => "forget",
            MemoryRequest::RecordDecision { .. } => "record_decision",
            MemoryRequest::RecordEdit { .. } => "record_edit",
            MemoryRequest::Status { .. } => "status",
        }
    }

    /// True when this op needs the embedder staged before a direct-open call
    /// (only `recall` touches vector search). The socket path never needs this
    /// — contextd's warm handle already resolved the resident embedder.
    pub fn needs_embedder(&self) -> bool {
        matches!(self, MemoryRequest::Recall { .. })
    }
}

/// Engine-op response: the tool's own DTO as JSON on success, a tool-native
/// error `code` (plus a human `message`) on failure. The mcp proxy parses this
/// off the socket and re-wraps `data` into the matching rmcp `Json<T>` DTO, or
/// maps `code` back to an `rmcp::ErrorData`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MemoryResponse {
    Ok { data: Value },
    Err { code: String, message: String },
}

/// The ONE variant→handler dispatch. Both the contextd socket path and the mcp
/// direct-open fallback call this against a ready engine handle, so the two
/// surfaces execute byte-identical engine logic. Staging (recall only) is the
/// CALLER's concern — see [`MemoryRequest::needs_embedder`].
pub async fn dispatch(
    lunaris: &Lunaris,
    scope: &Scope,
    request: MemoryRequest,
) -> Result<Value, ServiceError> {
    match request {
        MemoryRequest::Ingest { params, .. } => {
            to_value(crate::ingest::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Recall { params, .. } => {
            to_value(crate::recall::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Forget { params, .. } => {
            to_value(crate::forget::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::RecordDecision { params, .. } => {
            to_value(crate::record_decision::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::RecordEdit { params, .. } => {
            to_value(crate::record_edit::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Status { .. } => {
            to_value(crate::status::handle(lunaris, scope, crate::status::StatusParams {}).await?)
        }
    }
}

/// Serialize a tool DTO to JSON. Our DTOs are plain structs, so this only fails
/// on a non-finite float (e.g. a NaN score) — surfaced as `InvalidInput` rather
/// than a panic on the socket/proxy path.
fn to_value<T: Serialize>(dto: T) -> Result<Value, ServiceError> {
    serde_json::to_value(dto)
        .map_err(|e| ServiceError::InvalidInput(format!("response serialization failed: {e}")))
}
