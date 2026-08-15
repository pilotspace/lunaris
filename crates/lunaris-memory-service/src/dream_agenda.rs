//! `memory.dream_agenda` — READ-ONLY distillation planner (engram-soul-loop
//! **task 8a**, `.add/tasks/dream-agenda/TASK.md` §3 CONTRACT, frozen).
//!
//! Tin's locked decision: the coding harness (Claude, not Lunaris) is the
//! distiller/judge. This tool ONLY surfaces candidate clusters of ripe raw
//! episodes with activation stats for the harness to reason over — it writes
//! nothing (no episode, no ledger, no agenda row). Pair with the task-8b
//! `memory.distill` transactional apply tool (not yet built) to act on a
//! cluster.
//!
//! Thin wrapper over [`lunaris::ScopedLunaris::dream_agenda`], which in turn
//! calls the read-only `lunaris_consolidate::dream::build_dream_agenda`
//! engine. This handler's own job is param validation (producing the frozen
//! §3 CONTRACT reject codes) + DTO shaping — it never touches storage
//! directly.
//!
//! The `scope` argument is the **only** partition key; the wire DTO
//! intentionally carries no `scope` or `tenant` field (CLAUDE.md DTO
//! discipline).

use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;

/// Default `limit` when the caller omits it — matches `.add/tasks/
/// dream-agenda/TASK.md` §3 CONTRACT.
const DEFAULT_LIMIT: usize = 20;
/// Default `min_cluster_size` when the caller omits it.
const DEFAULT_MIN_CLUSTER_SIZE: usize = 1;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.dream_agenda`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DreamAgendaParams {
    /// Maximum number of clusters to return. Defaults to 20; `0` or `>100`
    /// is rejected as `"invalid_limit"`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Minimum cluster size to keep. Defaults to 1; `>100` is rejected as
    /// `"invalid_min_cluster_size"`.
    #[serde(default)]
    pub min_cluster_size: Option<usize>,
    /// Optional ripeness ceiling — only candidates whose recomputed
    /// activation is `<=` this value are considered. `NaN` is rejected as
    /// `"invalid_max_activation"`.
    #[serde(default)]
    pub max_activation: Option<f64>,
}

/// One candidate distillation cluster.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DreamClusterDto {
    /// Stable cluster id — `"com:<hex>"` (leiden community) or
    /// `"src:<class>"` (source-class fallback).
    pub cluster_id: String,
    /// `member_episode_ids.len()`.
    pub size: usize,
    /// Member episode ULIDs (string form), sorted.
    pub member_episode_ids: Vec<String>,
    pub mean_activation: f64,
    pub max_activation: f64,
    /// Most-common source-class among the cluster's members.
    pub dominant_source: String,
    /// Up to 3 member content snippets, each trimmed to <=280 chars.
    pub snippets: Vec<String>,
}

/// Output of a successful `memory.dream_agenda` call. FLAT root (CLAUDE.md
/// MCP response-schema invariant — never a `#[serde(tag)]` enum).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DreamAgendaResponse {
    /// Always `"ok"` on success (errors surface as a 4xx `ServiceError`).
    pub status: String,
    /// Candidates considered after exclusions/filter (before clustering).
    pub total_candidates: usize,
    /// `clusters.len()`.
    pub count: usize,
    pub clusters: Vec<DreamClusterDto>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.dream_agenda`. READ-ONLY — never calls `atomic_write`.
///
/// Validates `params` FIRST (§1 Reject) — an invalid `limit` /
/// `min_cluster_size` / `max_activation` never reaches the engine, so no
/// storage call happens for a rejected request.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: DreamAgendaParams,
) -> Result<DreamAgendaResponse, ServiceError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > 100 {
        return Err(ServiceError::InvalidInput("invalid_limit".to_string()));
    }
    let min_cluster_size = params.min_cluster_size.unwrap_or(DEFAULT_MIN_CLUSTER_SIZE);
    if min_cluster_size > 100 {
        return Err(ServiceError::InvalidInput("invalid_min_cluster_size".to_string()));
    }
    if params.max_activation.is_some_and(f64::is_nan) {
        return Err(ServiceError::InvalidInput("invalid_max_activation".to_string()));
    }

    let cfg = lunaris::DreamConfig {
        limit,
        min_cluster_size,
        max_activation: params.max_activation,
        decay: lunaris_core::activation::DEFAULT_DECAY,
    };

    let scoped = lunaris.scoped(scope.clone());
    let agenda = scoped.dream_agenda(cfg).await?;

    let clusters: Vec<DreamClusterDto> = agenda
        .clusters
        .into_iter()
        .map(|c| DreamClusterDto {
            cluster_id: c.cluster_id,
            size: c.member_ids.len(),
            member_episode_ids: c.member_ids.iter().map(ToString::to_string).collect(),
            mean_activation: c.mean_activation,
            max_activation: c.max_activation,
            dominant_source: c.dominant_source,
            snippets: c.snippets,
        })
        .collect();

    tracing::debug!(
        scope = scope.as_str(),
        total_candidates = agenda.total_candidates,
        clusters = clusters.len(),
        "memory.dream_agenda listed",
    );

    Ok(DreamAgendaResponse {
        status: "ok".to_string(),
        total_candidates: agenda.total_candidates,
        count: clusters.len(),
        clusters,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris::{EpisodeBuilder, FactInput, StructuredIngest};
    use lunaris_core::StubEmbedder;
    use lunaris_core::activation::{Grain, RefSignal, Strength};
    use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};
    use std::sync::Arc;
    use ulid::Ulid;

    /// Ported off `memory://` (0.7.0 prerequisite) onto a harness-issued
    /// ephemeral Moon; falls back to `memory://` only where no Moon binary
    /// exists. `TestEngine` derefs to `Lunaris`, so the call sites below are
    /// unchanged — but the binding owns the Moon child and must outlive them.
    async fn make_engine() -> (TestEngine, Scope) {
        // `StubEmbedder` (deterministic 768-d vectors) so the ingest_structured
        // calls below never fall back to a NoopEmbedder / zero-vector path on
        // CI (mirrors `resolve.rs` / `verify_agenda.rs`'s test harness).
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = open_test_engine_with_embedder(embedder).await;
        let scope = Scope::new(format!("test.dream-agenda-{}", Ulid::new())).unwrap();
        (lunaris, scope)
    }

    fn fact(subject: &str, predicate: &str, object: &str, text: &str) -> FactInput {
        FactInput {
            fact_text: text.to_string(),
            subject_name: subject.to_string(),
            subject_type: "concept".to_string(),
            predicate: predicate.to_string(),
            object_name: object.to_string(),
            object_type: "concept".to_string(),
            confidence: 1.0,
            valid_from: chrono::Utc::now(),
            valid_to: None,
        }
    }

    /// Production-wiring proof (feedback_built_not_wired.md convention):
    /// `ScopedLunaris::ingest_structured` (no LLM) creates real facts/
    /// entities, `record_activation_refs` seeds the ledger, and
    /// `memory.dream_agenda` — driven through the FULL wrapper -> service
    /// stack — surfaces a leiden-clustered agenda. Proves `leiden_pass`
    /// fires through production wiring, not just the engine's own unit
    /// tests.
    #[tokio::test]
    async fn dream_agenda_clusters_via_ingest_structured_and_ledger() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let ep_a = Ulid::new();
        let ep_b = Ulid::new();

        scoped
            .ingest_structured(
                StructuredIngest::new(EpisodeBuilder::new("lunaris:a", "episode a text").id(ep_a))
                    .with_facts(vec![fact("amber-relay", "relates_to", "foo-a", "amber -> foo-a")]),
            )
            .await
            .expect("ingest_structured a");

        scoped
            .ingest_structured(
                StructuredIngest::new(EpisodeBuilder::new("lunaris:b", "episode b text").id(ep_b))
                    .with_facts(vec![fact("amber-relay", "relates_to", "foo-b", "amber -> foo-b")]),
            )
            .await
            .expect("ingest_structured b");

        scoped
            .record_activation_refs(&[
                RefSignal { id: ep_a, grain: Grain::Turn, strength: Strength::Weak },
                RefSignal { id: ep_b, grain: Grain::Turn, strength: Strength::Weak },
            ])
            .await
            .expect("record_activation_refs");

        let resp = handle(
            &lunaris,
            &scope,
            DreamAgendaParams { limit: None, min_cluster_size: None, max_activation: None },
        )
        .await
        .expect("dream_agenda must succeed");

        assert_eq!(resp.status, "ok");
        assert_eq!(resp.total_candidates, 2);
        let cluster = resp
            .clusters
            .iter()
            .find(|c| c.member_episode_ids.contains(&ep_a.to_string()))
            .expect("a cluster containing ep_a must exist");
        assert!(
            cluster.cluster_id.starts_with("com:"),
            "leiden path must fire end-to-end through the full stack: {}",
            cluster.cluster_id
        );
        assert!(cluster.member_episode_ids.contains(&ep_b.to_string()));
        assert_eq!(cluster.size, 2);
    }

    #[tokio::test]
    async fn reject_invalid_limit_zero_and_over_cap() {
        let (lunaris, scope) = make_engine().await;
        for limit in [0usize, 101] {
            let err = handle(
                &lunaris,
                &scope,
                DreamAgendaParams {
                    limit: Some(limit),
                    min_cluster_size: None,
                    max_activation: None,
                },
            )
            .await
            .unwrap_err();
            match err {
                ServiceError::InvalidInput(msg) => assert_eq!(msg, "invalid_limit"),
                other => panic!("expected InvalidInput(invalid_limit), got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn reject_invalid_min_cluster_size() {
        let (lunaris, scope) = make_engine().await;
        let err = handle(
            &lunaris,
            &scope,
            DreamAgendaParams { limit: None, min_cluster_size: Some(101), max_activation: None },
        )
        .await
        .unwrap_err();
        match err {
            ServiceError::InvalidInput(msg) => assert_eq!(msg, "invalid_min_cluster_size"),
            other => panic!("expected InvalidInput(invalid_min_cluster_size), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reject_nan_max_activation() {
        let (lunaris, scope) = make_engine().await;
        let err = handle(
            &lunaris,
            &scope,
            DreamAgendaParams {
                limit: None,
                min_cluster_size: None,
                max_activation: Some(f64::NAN),
            },
        )
        .await
        .unwrap_err();
        match err {
            ServiceError::InvalidInput(msg) => assert_eq!(msg, "invalid_max_activation"),
            other => panic!("expected InvalidInput(invalid_max_activation), got {other:?}"),
        }
    }

    /// `deny_unknown_fields` rejects a smuggled `scope` field.
    #[test]
    fn params_reject_smuggled_scope() {
        let raw = serde_json::json!({"limit": 5, "scope": "other"});
        let parsed: Result<DreamAgendaParams, _> = serde_json::from_value(raw);
        assert!(parsed.is_err(), "deny_unknown_fields must reject a smuggled scope");
    }

    /// An empty scope (no ledger references) returns an empty agenda, not an
    /// error.
    #[tokio::test]
    async fn empty_scope_returns_empty_agenda() {
        let (lunaris, scope) = make_engine().await;
        let resp = handle(
            &lunaris,
            &scope,
            DreamAgendaParams { limit: None, min_cluster_size: None, max_activation: None },
        )
        .await
        .expect("dream_agenda on an empty scope must succeed");
        assert_eq!(resp.total_candidates, 0);
        assert_eq!(resp.count, 0);
        assert!(resp.clusters.is_empty());
    }
}
