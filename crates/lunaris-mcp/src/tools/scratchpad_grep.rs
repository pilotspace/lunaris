//! `memory.scratchpad_grep` — grep scratchpad keys by prefix pattern.
//!
//! INGEST-04 invariant: this handler NEVER calls `atomic_write`.
//! `! grep -q 'atomic_write' crates/lunaris-mcp/src/tools/scratchpad_grep.rs` must exit 0.
//!
//! Returns all entries whose full source key starts with `{namespace}{pattern}`.
//! The `source` field in ScratchpadEntry is the FULL source key as stored
//! (i.e. `{namespace}{key}`) — it is NOT stripped.
//!
//! ## Embedded-backend keyword fallback (owned by the primitive)
//!
//! Same as scratchpad_read: `WorkingMemory::grep` builds a fused
//! Vector+Keyword(BM25) plan and falls back to vector-only when the backend's
//! `keyword_search` is `NotSupported`, with `Filter::StartsWith` on `source`
//! enforced at the SQL boundary. That fallback, and the recovery of each
//! verbatim value from the parent Episode `content`, both live INSIDE the
//! primitive (a single find + recover path). This handler just calls `wm.grep`.
//!
//! Moon indexes synchronously inline in HSET, so write-then-grep is
//! read-your-writes consistent on the same shard — no FT-index lag. The sqlite
//! backend is likewise synchronous via `Filter::StartsWith` at the SQL boundary.

use lunaris::WorkingMemory;
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_grep`.
///
/// `#[serde(deny_unknown_fields)]` mandatory (CLAUDE.md §HTTP DTO discipline).
/// `scope` is absent — bound at server startup, never on the wire.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchpadGrepParams {
    /// Pattern to match against source key suffix (StartsWith: `{namespace}{pattern}*`).
    pub pattern: String,
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// A single matched scratchpad entry.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScratchpadEntry {
    /// Full source key (namespace + key, e.g. "scratchpad/my-key").
    pub source: String,
    /// The stored JSON value.
    pub value: serde_json::Value,
}

/// Output of a successful `memory.scratchpad_grep` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScratchpadGrepResponse {
    /// Matched entries, each with the full source key and stored value.
    pub entries: Vec<ScratchpadEntry>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.scratchpad_grep`.
pub(crate) async fn handle(
    state: &AppState,
    params: ScratchpadGrepParams,
) -> Result<ScratchpadGrepResponse, ToolError> {
    crate::tools::staging::maybe_ensure_staged().await?;
    let namespace = crate::tools::staging::resolve_namespace(params.namespace)?;
    let wm = WorkingMemory::new(state.lunaris.clone(), state.scope.clone(), namespace);

    // `WorkingMemory::grep` owns the fused→vector-only fallback AND recovers each
    // verbatim value from the parent Episode `content` — see the module doc.
    let pairs = wm.grep(&params.pattern).await.map_err(ToolError::LunarisEngine)?;
    let entries =
        pairs.into_iter().map(|(source, value)| ScratchpadEntry { source, value }).collect();
    Ok(ScratchpadGrepResponse { entries })
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
        AppState { lunaris: Arc::new(lunaris), scope }
    }

    async fn write_key(state: &AppState, key: &str, value: serde_json::Value, namespace: &str) {
        scratchpad_write::handle(
            state,
            scratchpad_write::ScratchpadWriteParams {
                key: key.into(),
                value,
                namespace: Some(namespace.into()),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn grep_returns_written_entries() {
        let state = fresh_state("test-sg-written").await;

        write_key(&state, "alpha", json!("a"), "grep-test-ns-a/").await;
        write_key(&state, "beta", json!("b"), "grep-test-ns-a/").await;

        let resp = handle(
            &state,
            ScratchpadGrepParams { pattern: "".into(), namespace: Some("grep-test-ns-a/".into()) },
        )
        .await
        .unwrap();

        assert!(
            resp.entries.len() >= 2,
            "grep must return at least 2 entries; got: {}",
            resp.entries.len()
        );
        let sources: Vec<&str> = resp.entries.iter().map(|e| e.source.as_str()).collect();
        assert!(
            sources.iter().any(|s| s.contains("alpha")),
            "entries must include alpha; got: {sources:?}"
        );
        assert!(
            sources.iter().any(|s| s.contains("beta")),
            "entries must include beta; got: {sources:?}"
        );
    }

    #[tokio::test]
    async fn grep_cross_namespace_isolation() {
        let state = fresh_state("test-sg-isolation").await;

        write_key(&state, "shared-key", json!("from-a"), "ns-iso-a/").await;
        write_key(&state, "other-key", json!("from-b"), "ns-iso-b/").await;

        let resp = handle(
            &state,
            ScratchpadGrepParams { pattern: "".into(), namespace: Some("ns-iso-a/".into()) },
        )
        .await
        .unwrap();

        for entry in &resp.entries {
            assert!(
                !entry.source.starts_with("ns-iso-b/"),
                "grep under ns-iso-a/ must not return ns-iso-b/ entries; got source: {}",
                entry.source
            );
        }
    }

    #[tokio::test]
    async fn grep_invalid_namespace_colon() {
        let state = fresh_state("test-sg-ns").await;
        let result = handle(
            &state,
            ScratchpadGrepParams { pattern: "".into(), namespace: Some("x:y".into()) },
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(_))),
            "namespace with ':' must return InvalidInput; got: {result:?}"
        );
    }
}
