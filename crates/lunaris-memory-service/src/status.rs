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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueStatus {
    pub topic: String,
    pub available: bool,
    pub depth: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    use lunaris_core::Scope;
    use lunaris_test_harness::open_test_engine;

    use super::*;

    /// Was `status_reports_non_native_queue_for_sqlite`, which opened a
    /// `sqlite://` temp file to exercise the degraded branch. 0.7.0 deleted
    /// that backend and with it the only substrate whose queue was not
    /// native, so the negative case is unreachable — asserting it would need
    /// a hand-rolled `StoragePort` double reporting a capability no shipped
    /// backend reports. What is worth pinning is the shape `handle` returns
    /// on the path that actually runs: three probed topics, all live.
    #[tokio::test]
    async fn status_reports_a_native_queue_and_probes_every_topic() {
        let engine = open_test_engine().await;
        let scope = Scope::new("mcp-status-test").unwrap();

        let response = handle(&engine, &scope, StatusParams {}).await.unwrap();

        assert!(response.queue_native, "Moon's queue is native");
        assert_eq!(response.queues.len(), 3, "one entry per probed topic");
        for queue in &response.queues {
            assert!(
                queue.available,
                "topic `{}` must probe live on Moon: {:?}",
                queue.topic, queue.error
            );
            assert!(queue.error.is_none(), "no error on an available topic");
        }
    }
}
