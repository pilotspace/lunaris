//! `memory.scratchpad_write` — write a key-value pair to the agent scratchpad.
//!
//! Transport-neutral (contextd-mcp-merge scratchpad-proxiable task): moved from
//! `lunaris-mcp/src/tools/scratchpad_write.rs`. The mcp `#[tool]` wrapper
//! pre-resolves the session-aware namespace into `params.namespace` and routes
//! here through the proxy; contextd's socket dispatch calls the same handler.
//!
//! INGEST-04 invariant: this handler MUST NOT call `atomic_write` directly.
//! It calls `WorkingMemory::write` which rides `Lunaris::ingest` → one atomic_write.
//!
//! `namespace` is a source-key prefix, NOT a security boundary — scope is.

use std::sync::Arc;

use lunaris::{Lunaris, WorkingMemory};
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};

use crate::ServiceError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_write`.
///
/// `#[serde(deny_unknown_fields)]` mandatory (CLAUDE.md §HTTP DTO discipline) —
/// blocks a wire payload smuggling a `scope`/`tenant` override past the
/// server-bound partition key.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScratchpadWriteParams {
    /// Key under which to store the value (e.g. "current-task", "draft-plan").
    pub key: String,
    /// JSON value to store.
    pub value: serde_json::Value,
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Pre-resolved to `Some(..)` session-aware by the mcp caller.
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Output of a successful `memory.scratchpad_write` call.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadWriteResponse {
    /// Log-sequence number of the committed write (wall_ms:counter).
    pub lsn: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.scratchpad_write`.
///
/// Write does NOT need the embedder staged — `WorkingMemory::write` rides
/// ingest, not recall (see `MemoryRequest::needs_embedder`).
pub async fn handle(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    params: ScratchpadWriteParams,
) -> Result<ScratchpadWriteResponse, ServiceError> {
    let namespace = crate::namespace::resolve(params.namespace)?;
    let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), namespace);
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

    async fn fresh(scope_name: &str) -> (Arc<Lunaris>, Scope) {
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Arc::new(Lunaris::open_with_embedder("memory://", embedder).await.unwrap());
        let scope = Scope::new(scope_name).unwrap();
        (lunaris, scope)
    }

    #[tokio::test]
    async fn scratchpad_write_returns_valid_lsn() {
        let (lunaris, scope) = fresh("test-sw-lsn").await;
        let resp = handle(
            &lunaris,
            &scope,
            ScratchpadWriteParams {
                key: "task".into(),
                value: json!("do the thing"),
                namespace: None,
            },
        )
        .await
        .unwrap();
        let parts: Vec<&str> = resp.lsn.split(':').collect();
        assert_eq!(parts.len(), 2, "lsn must be wall_ms:counter; got: {}", resp.lsn);
        assert!(parts[0].parse::<u64>().is_ok(), "wall_ms must be numeric; got: {}", parts[0]);
        assert!(parts[1].parse::<u64>().is_ok(), "counter must be numeric; got: {}", parts[1]);
    }

    #[tokio::test]
    async fn scratchpad_write_with_namespace_returns_valid_lsn() {
        let (lunaris, scope) = fresh("test-sw-ns").await;
        let resp = handle(
            &lunaris,
            &scope,
            ScratchpadWriteParams {
                key: "note".into(),
                value: json!({"text": "hello"}),
                namespace: Some("notes/".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resp.lsn.split(':').count(),
            2,
            "lsn must be wall_ms:counter; got: {}",
            resp.lsn
        );
    }

    #[tokio::test]
    async fn scratchpad_write_invalid_namespace_colon() {
        let (lunaris, scope) = fresh("test-sw-colon").await;
        let result = handle(
            &lunaris,
            &scope,
            ScratchpadWriteParams {
                key: "k".into(),
                value: json!(1),
                namespace: Some("bad:ns".into()),
            },
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::InvalidInput(_))),
            "namespace with ':' must return InvalidInput; got: {result:?}"
        );
    }

    #[tokio::test]
    async fn scratchpad_write_empty_namespace_rejected() {
        let (lunaris, scope) = fresh("test-sw-empty").await;
        let result = handle(
            &lunaris,
            &scope,
            ScratchpadWriteParams {
                key: "k".into(),
                value: json!(1),
                namespace: Some(String::new()),
            },
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::InvalidInput(_))),
            "empty namespace must return InvalidInput; got: {result:?}"
        );
    }

    /// deny_unknown_fields blocks a smuggled `scope` override on the wire params.
    #[test]
    fn params_reject_smuggled_scope_field() {
        let raw = json!({"key": "k", "value": 1, "scope": "other"});
        let parsed: Result<ScratchpadWriteParams, _> = serde_json::from_value(raw);
        assert!(parsed.is_err(), "deny_unknown_fields must reject a smuggled `scope`");
    }
}
