//! `memory.scratchpad_read` — read a single key from the agent scratchpad.
//!
//! INGEST-04 invariant: this handler NEVER calls `atomic_write`.
//! `! grep -q 'atomic_write' crates/lunaris-mcp/src/tools/scratchpad_read.rs` must exit 0.
//!
//! Requires the embedder to be staged (calls `maybe_ensure_staged` lazily — same
//! staging path as `memory.recall`, never a second path).
//!
//! ## Embedded-backend keyword fallback (owned by the primitive)
//!
//! `WorkingMemory::read` builds a fused Vector+Keyword(BM25) plan internally and
//! falls back to vector-only when the backend's `keyword_search` is
//! `NotSupported` (the embedded/sqlite default — the `Filter::Eq` on `source`
//! is enforced at the SQL boundary). That fallback, and the recovery of the
//! verbatim value from the parent Episode `content`, both live INSIDE the
//! primitive (a single find + recover path). This handler therefore just calls
//! `wm.read` — it does NOT re-implement a second fallback or reconstruct the
//! value from the lossy chunk `text`.
//!
//! Moon note: Moon indexes synchronously inline in HSET, so write-then-read is
//! read-your-writes consistent on the same shard — no FT-index lag. The sqlite
//! backend is likewise synchronous via `Filter::Eq` at the SQL boundary.

use lunaris::WorkingMemory;
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_read`.
///
/// `#[serde(deny_unknown_fields)]` mandatory (CLAUDE.md §HTTP DTO discipline).
/// `scope` is absent — bound at server startup, never on the wire.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchpadReadParams {
    /// Key to look up.
    pub key: String,
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Output of a successful `memory.scratchpad_read` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScratchpadReadResponse {
    /// Whether the key was found.
    pub found: bool,
    /// The stored JSON value, or null if not found.
    pub value: Option<serde_json::Value>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.scratchpad_read`.
pub(crate) async fn handle(
    state: &AppState,
    params: ScratchpadReadParams,
) -> Result<ScratchpadReadResponse, ToolError> {
    crate::tools::staging::maybe_ensure_staged().await?;
    let namespace =
        crate::tools::staging::resolve_namespace_session_aware(state, params.namespace).await?;
    let wm = WorkingMemory::new(state.lunaris.clone(), state.scope.clone(), namespace);

    // `WorkingMemory::read` owns the fused→vector-only fallback AND recovers the
    // verbatim value from the parent Episode `content` — see the module doc.
    let value = wm.read(&params.key).await.map_err(ToolError::LunarisEngine)?;
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
    use crate::state::AppState;
    use crate::tools::scratchpad_write;

    async fn fresh_state(scope_name: &str) -> AppState {
        crate::tools::staging::skip_stage_for_tests();
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
        let scope = Scope::new(scope_name).unwrap();
        AppState {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None,
        }
    }

    /// Discriminating test: write then read — proves vector-only fallback works on
    /// the embedded backend (keyword_search returns NotSupported; handler falls back
    /// to Vector-only + Filter::Eq on source, which is enforced at SQL boundary).
    #[tokio::test]
    async fn write_then_read_round_trip() {
        let state = fresh_state("test-sr-rt").await;

        // Write via scratchpad_write handler
        scratchpad_write::handle(
            &state,
            scratchpad_write::ScratchpadWriteParams {
                key: "round-trip-key".into(),
                value: json!({"answer": 42}),
                namespace: None,
            },
        )
        .await
        .unwrap();

        // Read back via scratchpad_read handler
        let resp =
            handle(&state, ScratchpadReadParams { key: "round-trip-key".into(), namespace: None })
                .await
                .unwrap();

        assert!(resp.found, "write-then-read must return found=true");
        assert_eq!(resp.value, Some(json!({"answer": 42})), "value must survive the round trip");
    }

    #[tokio::test]
    async fn read_missing_key_returns_found_false() {
        let state = fresh_state("test-sr-miss").await;

        let resp =
            handle(&state, ScratchpadReadParams { key: "never-written".into(), namespace: None })
                .await
                .unwrap();

        assert!(!resp.found, "missing key must return found=false");
        assert_eq!(resp.value, None);
    }

    #[tokio::test]
    async fn read_invalid_namespace_colon() {
        // Validation fires before any IO — no state needed
        let state = fresh_state("test-sr-ns").await;
        let result = handle(
            &state,
            ScratchpadReadParams { key: "k".into(), namespace: Some("bad:ns".into()) },
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "namespace with ':' must return InvalidInput; got: {result:?}"
        );
    }
}
