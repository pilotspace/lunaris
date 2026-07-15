//! `memory.scratchpad_read` — read a key's verbatim value from the scratchpad.
//!
//! Transport-neutral (scratchpad-proxiable task): moved from
//! `lunaris-mcp/src/tools/scratchpad_read.rs`. Model staging is a CALLER
//! concern — `MemoryRequest::needs_embedder()` is true for read, so the mcp
//! direct-open path stages before dispatch and contextd stages at warm-up.
//! `WorkingMemory::read` owns the fused→vector-only fallback and recovers the
//! verbatim value from the parent Episode `content`.

use std::sync::Arc;

use lunaris::{Lunaris, WorkingMemory};
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};

use crate::ServiceError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_read`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScratchpadReadParams {
    /// Key to look up.
    pub key: String,
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Output of a successful `memory.scratchpad_read` call.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadReadResponse {
    /// Whether the key was found.
    pub found: bool,
    /// The stored JSON value, or null if not found.
    pub value: Option<serde_json::Value>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.scratchpad_read`.
pub async fn handle(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    params: ScratchpadReadParams,
) -> Result<ScratchpadReadResponse, ServiceError> {
    let namespace = crate::namespace::resolve(params.namespace)?;
    let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), namespace);
    let value = wm.read(&params.key).await?;
    Ok(ScratchpadReadResponse { found: value.is_some(), value })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::{Scope, StubEmbedder};
    use serde_json::json;

    use super::*;
    use crate::scratchpad_write;

    async fn fresh(scope_name: &str) -> (Arc<Lunaris>, Scope) {
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Arc::new(Lunaris::open_with_embedder("memory://", embedder).await.unwrap());
        let scope = Scope::new(scope_name).unwrap();
        (lunaris, scope)
    }

    /// Discriminating: write then read — proves the vector-only fallback works on
    /// the embedded backend (keyword_search NotSupported → Vector-only + Filter::Eq).
    #[tokio::test]
    async fn write_then_read_round_trip() {
        let (lunaris, scope) = fresh("test-sr-rt").await;
        scratchpad_write::handle(
            &lunaris,
            &scope,
            scratchpad_write::ScratchpadWriteParams {
                key: "round-trip-key".into(),
                value: json!({"answer": 42}),
                namespace: None,
            },
        )
        .await
        .unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            ScratchpadReadParams { key: "round-trip-key".into(), namespace: None },
        )
        .await
        .unwrap();

        assert!(resp.found, "write-then-read must return found=true");
        assert_eq!(resp.value, Some(json!({"answer": 42})), "value must survive the round trip");
    }

    #[tokio::test]
    async fn read_missing_key_returns_found_false() {
        let (lunaris, scope) = fresh("test-sr-miss").await;
        let resp = handle(
            &lunaris,
            &scope,
            ScratchpadReadParams { key: "never-written".into(), namespace: None },
        )
        .await
        .unwrap();
        assert!(!resp.found, "missing key must return found=false");
        assert_eq!(resp.value, None);
    }

    #[tokio::test]
    async fn read_invalid_namespace_colon() {
        let (lunaris, scope) = fresh("test-sr-ns").await;
        let result = handle(
            &lunaris,
            &scope,
            ScratchpadReadParams { key: "k".into(), namespace: Some("bad:ns".into()) },
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::InvalidInput(_))),
            "namespace with ':' must return InvalidInput; got: {result:?}"
        );
    }
}
