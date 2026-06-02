//! `memory.status` — report backend capabilities and queue health.

use lunaris_core::StoragePort;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{state::AppState, tools::ToolError};

const VERIFY_QUEUE_TOPIC: &str = "__lunaris_verify__";
const CONSOLIDATE_QUEUE_TOPIC: &str = "__lunaris_consolidate__";

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct StatusParams {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct QueueStatus {
    pub topic: String,
    pub available: bool,
    pub depth: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct StatusResponse {
    pub scope: String,
    pub queue_native: bool,
    pub graph_native: bool,
    pub rerank_native: bool,
    pub native_rrf: bool,
    pub max_vector_dim: u32,
    pub max_scopes_recommended: usize,
    pub cypher_dialect: String,
    pub queues: Vec<QueueStatus>,
}

pub(crate) async fn handle(
    state: &AppState,
    _params: StatusParams,
) -> Result<StatusResponse, ToolError> {
    let storage = state.lunaris.storage();
    let caps = storage.capabilities();
    let queues = if caps.queue_native {
        vec![
            queue_status(storage.as_ref(), &state.scope, VERIFY_QUEUE_TOPIC).await,
            queue_status(storage.as_ref(), &state.scope, CONSOLIDATE_QUEUE_TOPIC).await,
        ]
    } else {
        vec![
            QueueStatus {
                topic: VERIFY_QUEUE_TOPIC.to_string(),
                available: false,
                depth: None,
                error: Some("backend does not advertise native queue support".to_string()),
            },
            QueueStatus {
                topic: CONSOLIDATE_QUEUE_TOPIC.to_string(),
                available: false,
                depth: None,
                error: Some("backend does not advertise native queue support".to_string()),
            },
        ]
    };

    Ok(StatusResponse {
        scope: state.scope.as_str().to_string(),
        queue_native: caps.queue_native,
        graph_native: caps.graph_native,
        rerank_native: caps.rerank_native,
        native_rrf: caps.native_rrf,
        max_vector_dim: caps.max_vector_dim,
        max_scopes_recommended: caps.max_scopes_recommended,
        cypher_dialect: format!("{:?}", caps.cypher_dialect),
        queues,
    })
}

async fn queue_status(
    storage: &dyn StoragePort,
    scope: &lunaris_core::Scope,
    topic: &str,
) -> QueueStatus {
    match storage.queue_depth(scope, topic, 0).await {
        Ok(depth) => QueueStatus {
            topic: topic.to_string(),
            available: true,
            depth: Some(depth),
            error: None,
        },
        Err(err) => QueueStatus {
            topic: topic.to_string(),
            available: false,
            depth: None,
            error: Some(err.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::{Scope, StubEmbedder};

    use super::*;

    #[tokio::test]
    async fn status_reports_non_native_queue_for_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.db");
        let lunaris = Lunaris::open_with_embedder(
            &format!("sqlite://{}", path.display()),
            Arc::new(StubEmbedder::new(8)),
        )
        .await
        .unwrap();
        let state =
            AppState { lunaris: Arc::new(lunaris), scope: Scope::new("mcp-status-test").unwrap() };

        let response = handle(&state, StatusParams {}).await.unwrap();

        assert!(!response.queue_native);
        assert_eq!(response.queues.len(), 2);
        assert!(response.queues.iter().all(|queue| !queue.available));
    }
}
