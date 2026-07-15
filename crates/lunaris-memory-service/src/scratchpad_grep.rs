//! `memory.scratchpad_grep` — grep scratchpad keys by prefix pattern.
//!
//! Transport-neutral (scratchpad-proxiable task): moved from
//! `lunaris-mcp/src/tools/scratchpad_grep.rs`. Returns all entries whose full
//! source key starts with `{namespace}{pattern}`. Model staging is a CALLER
//! concern (`needs_embedder()` is true for grep). `WorkingMemory::grep` owns
//! the fused→vector-only fallback and per-entry verbatim recovery.

use std::sync::Arc;

use lunaris::{Lunaris, WorkingMemory};
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};

use crate::ServiceError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_grep`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScratchpadGrepParams {
    /// Pattern to match against source key suffix (StartsWith: `{namespace}{pattern}*`).
    pub pattern: String,
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// A single matched scratchpad entry.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadEntry {
    /// Full source key (namespace + key, e.g. "scratchpad/my-key").
    pub source: String,
    /// The stored JSON value.
    pub value: serde_json::Value,
}

/// Output of a successful `memory.scratchpad_grep` call.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadGrepResponse {
    /// Matched entries, each with the full source key and stored value.
    pub entries: Vec<ScratchpadEntry>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.scratchpad_grep`.
pub async fn handle(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    params: ScratchpadGrepParams,
) -> Result<ScratchpadGrepResponse, ServiceError> {
    let namespace = crate::namespace::resolve(params.namespace)?;
    let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), namespace);
    let pairs = wm.grep(&params.pattern).await?;
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
    use crate::scratchpad_write;

    async fn fresh(scope_name: &str) -> (Arc<Lunaris>, Scope) {
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Arc::new(Lunaris::open_with_embedder("memory://", embedder).await.unwrap());
        let scope = Scope::new(scope_name).unwrap();
        (lunaris, scope)
    }

    async fn write_key(
        lunaris: &Arc<Lunaris>,
        scope: &Scope,
        key: &str,
        value: serde_json::Value,
        namespace: &str,
    ) {
        scratchpad_write::handle(
            lunaris,
            scope,
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
        let (lunaris, scope) = fresh("test-sg-written").await;
        write_key(&lunaris, &scope, "alpha", json!("a"), "grep-test-ns-a/").await;
        write_key(&lunaris, &scope, "beta", json!("b"), "grep-test-ns-a/").await;

        let resp = handle(
            &lunaris,
            &scope,
            ScratchpadGrepParams { pattern: "".into(), namespace: Some("grep-test-ns-a/".into()) },
        )
        .await
        .unwrap();

        assert!(
            resp.entries.len() >= 2,
            "grep must return >=2 entries; got: {}",
            resp.entries.len()
        );
        let sources: Vec<&str> = resp.entries.iter().map(|e| e.source.as_str()).collect();
        assert!(
            sources.iter().any(|s| s.contains("alpha")),
            "must include alpha; got: {sources:?}"
        );
        assert!(sources.iter().any(|s| s.contains("beta")), "must include beta; got: {sources:?}");
    }

    #[tokio::test]
    async fn grep_cross_namespace_isolation() {
        let (lunaris, scope) = fresh("test-sg-isolation").await;
        write_key(&lunaris, &scope, "shared-key", json!("from-a"), "ns-iso-a/").await;
        write_key(&lunaris, &scope, "other-key", json!("from-b"), "ns-iso-b/").await;

        let resp = handle(
            &lunaris,
            &scope,
            ScratchpadGrepParams { pattern: "".into(), namespace: Some("ns-iso-a/".into()) },
        )
        .await
        .unwrap();

        for entry in &resp.entries {
            assert!(
                !entry.source.starts_with("ns-iso-b/"),
                "grep under ns-iso-a/ must not return ns-iso-b/ entries; got: {}",
                entry.source
            );
        }
    }

    #[tokio::test]
    async fn grep_invalid_namespace_colon() {
        let (lunaris, scope) = fresh("test-sg-ns").await;
        let result = handle(
            &lunaris,
            &scope,
            ScratchpadGrepParams { pattern: "".into(), namespace: Some("x:y".into()) },
        )
        .await;
        assert!(
            matches!(result, Err(ServiceError::InvalidInput(_))),
            "namespace with ':' must return InvalidInput; got: {result:?}"
        );
    }
}
