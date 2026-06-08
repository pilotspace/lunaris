//! `memory.scratchpad_write` — write a key-value pair to the agent scratchpad.
//!
//! INGEST-04 invariant: this handler MUST NOT call `atomic_write` directly.
//! It calls `WorkingMemory::write` which rides `Lunaris::ingest` → one atomic_write.
//! `! grep -q 'atomic_write' crates/lunaris-mcp/src/tools/scratchpad_write.rs` must exit 0.
//!
//! The scope comes from `AppState::scope` (server startup). Wire payloads MUST NOT
//! supply or override the scope — CLAUDE.md DTO discipline.
//! `namespace` is a source-key prefix, NOT a security boundary.

use lunaris::WorkingMemory;
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_write`.
///
/// `#[serde(deny_unknown_fields)]` mandatory (CLAUDE.md §HTTP DTO discipline).
/// `scope` is absent — bound at server startup, never on the wire.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchpadWriteParams {
    /// Key under which to store the value (e.g. "current-task", "draft-plan").
    pub key: String,
    /// JSON value to store.
    pub value: serde_json::Value,
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    /// This is a namespace, NOT a security boundary — scope is bound at startup.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Output of a successful `memory.scratchpad_write` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScratchpadWriteResponse {
    /// Log-sequence number of the committed write (wall_ms:counter).
    pub lsn: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.scratchpad_write`.
///
/// Note: write does NOT call `maybe_ensure_staged()` — only read/grep do
/// (write rides ingest, not recall).
/// Note: write does NOT need the keyword-NotSupported fallback —
/// `WorkingMemory::write` calls ingest, not recall.
pub(crate) async fn handle(
    state: &AppState,
    params: ScratchpadWriteParams,
) -> Result<ScratchpadWriteResponse, ToolError> {
    let namespace = crate::tools::staging::resolve_namespace(params.namespace)?;
    let wm = WorkingMemory::new(state.lunaris.clone(), state.scope.clone(), namespace);
    let lsn = wm.write(&params.key, params.value).await?;
    Ok(ScratchpadWriteResponse { lsn: lsn.to_string() })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::{Scope, StubEmbedder};
    use serde_json::json;

    use super::*;
    use crate::state::AppState;

    async fn fresh_state(scope_name: &str) -> AppState {
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
        let scope = Scope::new(scope_name).unwrap();
        AppState { lunaris: Arc::new(lunaris), scope }
    }

    #[tokio::test]
    async fn scratchpad_write_returns_valid_lsn() {
        let state = fresh_state("test-sw-lsn").await;
        let resp = handle(
            &state,
            ScratchpadWriteParams {
                key: "task".into(),
                value: json!("do the thing"),
                namespace: None,
            },
        )
        .await
        .unwrap();
        // lsn must match /^\d+:\d+$/
        let parts: Vec<&str> = resp.lsn.split(':').collect();
        assert_eq!(parts.len(), 2, "lsn must be wall_ms:counter; got: {}", resp.lsn);
        assert!(parts[0].parse::<u64>().is_ok(), "wall_ms must be numeric; got: {}", parts[0]);
        assert!(parts[1].parse::<u64>().is_ok(), "counter must be numeric; got: {}", parts[1]);
    }

    #[tokio::test]
    async fn scratchpad_write_with_namespace_returns_valid_lsn() {
        let state = fresh_state("test-sw-ns").await;
        let resp = handle(
            &state,
            ScratchpadWriteParams {
                key: "note".into(),
                value: json!({"text": "hello"}),
                namespace: Some("notes/".into()),
            },
        )
        .await
        .unwrap();
        let parts: Vec<&str> = resp.lsn.split(':').collect();
        assert_eq!(parts.len(), 2, "lsn must be wall_ms:counter; got: {}", resp.lsn);
    }

    #[tokio::test]
    async fn scratchpad_write_invalid_namespace_colon() {
        let state = fresh_state("test-sw-colon").await;
        let result = handle(
            &state,
            ScratchpadWriteParams {
                key: "k".into(),
                value: json!(1),
                namespace: Some("bad:ns".into()),
            },
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "namespace with ':' must return InvalidInput; got: {result:?}"
        );
    }

    #[tokio::test]
    async fn scratchpad_write_empty_namespace_rejected() {
        let state = fresh_state("test-sw-empty").await;
        let result = handle(
            &state,
            ScratchpadWriteParams {
                key: "k".into(),
                value: json!(1),
                namespace: Some(String::new()),
            },
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "empty namespace must return InvalidInput; got: {result:?}"
        );
    }
}
