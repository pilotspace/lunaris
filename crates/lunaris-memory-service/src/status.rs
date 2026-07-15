//! `memory.status` — report backend capabilities and queue health.

use lunaris_core::StoragePort;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;

const VERIFY_QUEUE_TOPIC: &str = "__lunaris_verify__";
const CONSOLIDATE_QUEUE_TOPIC: &str = "__lunaris_consolidate__";
const EMBED_QUEUE_TOPIC: &str = "__lunaris_embed__";

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StatusParams {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct QueueStatus {
    pub topic: String,
    pub available: bool,
    pub depth: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
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

pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    _params: StatusParams,
) -> Result<StatusResponse, ServiceError> {
    let storage = lunaris.storage();
    let caps = storage.capabilities();
    let queues = if caps.queue_native {
        vec![
            queue_status(storage.as_ref(), scope, EMBED_QUEUE_TOPIC).await,
            queue_status(storage.as_ref(), scope, VERIFY_QUEUE_TOPIC).await,
            queue_status(storage.as_ref(), scope, CONSOLIDATE_QUEUE_TOPIC).await,
        ]
    } else {
        vec![
            QueueStatus {
                topic: EMBED_QUEUE_TOPIC.to_string(),
                available: false,
                depth: None,
                error: Some("backend does not advertise native queue support".to_string()),
            },
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
        scope: scope.as_str().to_string(),
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
        let scope = Scope::new("mcp-status-test").unwrap();

        let response = handle(&lunaris, &scope, StatusParams {}).await.unwrap();

        assert!(!response.queue_native);
        assert_eq!(response.queues.len(), 3);
        assert!(response.queues.iter().all(|queue| !queue.available));
    }
}
