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

/// Every topic `memory.status` reports on.
///
/// One list, read by BOTH the live-probe branch and the
/// no-native-queue branch. It used to be two hand-written lists, so a topic
/// added to one and not the other would report on Moon and silently vanish
/// everywhere else.
/// `__lunaris_audit__` is here for a different reason than the other three,
/// and the difference is the point (Wave 6 / R2). The embed / verify /
/// consolidate topics each have a worker that drains them, so their depth is
/// a *backlog* — high means something is stuck. The audit topic has no
/// consumer at all: every mutation publishes to it, and its only reader
/// (`ScopedLunaris::audit_events`) is deliberately non-destructive. Moon's MQ
/// exposes no `TRIM` and no `MAXLEN` — CREATE / PUSH / POP / ACK / TRIGGER /
/// PUBLISH / LEN / DLQLEN is the whole surface — so it cannot be bounded from
/// this side at all. Its depth is not a backlog, it is the scope's total
/// mutation count, and it only ever goes up.
///
/// Bounding it needs a Moon feature. Reporting it does not, and it was the
/// one topic `memory.status` did not report.
const PROBED_TOPICS: &[&str] = &[
    EMBED_QUEUE_TOPIC,
    VERIFY_QUEUE_TOPIC,
    CONSOLIDATE_QUEUE_TOPIC,
    lunaris_core::audit::AUDIT_TOPIC,
];

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
    let mut queues = Vec::with_capacity(PROBED_TOPICS.len());
    for topic in PROBED_TOPICS {
        queues.push(if caps.queue_native {
            queue_status(storage.as_ref(), scope, topic).await
        } else {
            QueueStatus {
                topic: (*topic).to_string(),
                available: false,
                depth: None,
                error: Some("backend does not advertise native queue support".to_string()),
            }
        });
    }

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
        assert_eq!(response.queues.len(), PROBED_TOPICS.len(), "one entry per probed topic");
        for queue in &response.queues {
            assert!(
                queue.available,
                "topic `{}` must probe live on Moon: {:?}",
                queue.topic, queue.error
            );
            assert!(queue.error.is_none(), "no error on an available topic");
        }
    }

    /// Wave 6 / R2 — the audit topic must be among the probed ones.
    ///
    /// Nothing in Lunaris consumes `__lunaris_audit__`: every mutation
    /// publishes to it and the only reader, `ScopedLunaris::audit_events`, is
    /// explicitly non-destructive (it does not pop, ack, or advance a consumer
    /// group). Moon's MQ has no `TRIM` and no `MAXLEN` — its subcommands are
    /// CREATE / PUSH / POP / ACK / TRIGGER / PUBLISH / LEN / DLQLEN — so the
    /// topic cannot be bounded from this side at all.
    ///
    /// It therefore grows for the life of the scope, and until this test it
    /// was also the ONE topic `memory.status` did not report. The three it did
    /// probe are each drained by a worker. Bounding the growth needs a Moon
    /// feature; making it visible does not, and an operator who cannot see a
    /// number cannot act on it.
    #[tokio::test]
    async fn status_probes_the_unbounded_audit_topic() {
        let engine = open_test_engine().await;
        let scope = Scope::new("mcp-status-audit").unwrap();

        let response = handle(&engine, &scope, StatusParams {}).await.unwrap();

        let audit = response
            .queues
            .iter()
            .find(|q| q.topic == lunaris_core::audit::AUDIT_TOPIC)
            .unwrap_or_else(|| {
                panic!(
                    "memory.status does not probe `{}` — the only topic nothing drains, \
                     and the only one that grows without bound. Probed: {:?}",
                    lunaris_core::audit::AUDIT_TOPIC,
                    response.queues.iter().map(|q| &q.topic).collect::<Vec<_>>()
                )
            });
        assert!(audit.available, "the audit topic probed as unavailable: {:?}", audit.error);
        assert!(audit.depth.is_some(), "the audit topic reported no depth");
    }

    /// The degraded branch must cover the same topics as the live one. It used
    /// to be a hand-written second list, so a topic added to one and not the
    /// other would report on Moon and silently vanish on any backend without a
    /// native queue.
    #[test]
    fn every_probed_topic_is_named_once() {
        let mut seen = PROBED_TOPICS.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), PROBED_TOPICS.len(), "a topic is probed twice");
        assert!(
            PROBED_TOPICS.contains(&lunaris_core::audit::AUDIT_TOPIC),
            "PROBED_TOPICS lost the audit topic"
        );
    }
}
