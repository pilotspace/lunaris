//! `memory.resolve` — resolve one `memory.verify_agenda` entry: keep,
//! invalidate, or supersede.
//!
//! **DECISION (`.add/tasks/verify-agenda-tools/TASK.md` §0 GROUND, frozen):**
//! invalidate/supersede reuse `ScopedLunaris::forget(ForgetTarget::Id(id))`
//! — the same episode-scoped MVCC soft-delete tombstone `memory.forget`
//! already uses — NOT `lunaris_verify::reflect_apply` (which is fact-keyed,
//! `format!("fact:{ulid}")`, and unscoped; it cannot act on an episode
//! ULID). This is the ONLY already-scoped, already-episode-keyed tombstone
//! path in the codebase, so it honors the milestone's real intent ("reuse
//! an existing tombstone, don't fork a new one") better than the literal
//! `reflect_apply` name in the milestone doc.
//!
//! supersede in v1 does NOT re-stamp the old episode's payload with a typed
//! `superseded_by` provenance field (no such field exists yet) — the
//! replacement id is only echoed back in the response. Recorded as an open
//! spec delta in the frozen contract.
//!
//! The `scope` argument is the **only** partition key; the wire DTO
//! intentionally carries no `scope` or `tenant` field (CLAUDE.md DTO
//! discipline).

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::ServiceError;
use lunaris::{ForgetTarget, Lunaris};
use lunaris_core::Scope;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// What to do with a `memory.verify_agenda` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAction {
    /// Prune the agenda row; the episode stays live (not touched).
    Keep,
    /// Soft-tombstone the episode AND prune the agenda row, recording a
    /// replacement episode id (`superseded_by`, required).
    Supersede,
    /// Soft-tombstone the episode AND prune the agenda row.
    Invalidate,
}

/// Input parameters for `memory.resolve`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveParams {
    /// The stale-anchored episode's ULID (string form).
    pub episode_id: String,
    /// What to do with it.
    pub action: ResolveAction,
    /// Optional human-readable reason (audit-trail context; not persisted
    /// separately in v1 — see the module doc's spec-delta note).
    #[serde(default)]
    pub reason: Option<String>,
    /// Required when `action == supersede`: the replacement episode's ULID.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

/// Output of a successful `memory.resolve` call. FLAT root (CLAUDE.md MCP
/// response-schema invariant — never a `#[serde(tag)]` enum).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResolveResponse {
    /// One of `kept` | `invalidated` | `superseded` | `not_found`.
    pub status: String,
    /// The resolved episode's ULID (string form) — echoed back.
    pub episode_id: String,
    /// `true` iff the episode was soft-tombstoned (Invalidate / Supersede).
    /// Always `false` for Keep.
    pub invalidated: bool,
    /// `true` iff a verify_agenda row existed and was pruned.
    pub agenda_removed: bool,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.resolve`.
///
/// Validates `episode_id` as a ULID FIRST — a malformed id writes nothing
/// regardless of `action` (`.add/tasks/verify-agenda-tools/TASK.md` §1
/// Reject).
///
/// | `action`     | Effect                                                          |
/// |--------------|------------------------------------------------------------------|
/// | `keep`       | `remove_verify_agenda` only; `status` = kept / not_found          |
/// | `invalidate` | `forget(ForgetTarget::Id)` soft-delete THEN `remove_verify_agenda` |
/// | `supersede`  | requires a valid `superseded_by` ULID; else invalidate + `status=superseded` |
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: ResolveParams,
) -> Result<ResolveResponse, ServiceError> {
    let episode_id: Ulid = params.episode_id.parse().map_err(|e| {
        ServiceError::InvalidInput(format!(
            "episode_id is not a valid ULID: {e} (got {:?})",
            params.episode_id
        ))
    })?;

    let scoped = lunaris.scoped(scope.clone());

    match params.action {
        ResolveAction::Keep => {
            let agenda_removed = scoped.remove_verify_agenda(episode_id).await?;
            let status = if agenda_removed { "kept" } else { "not_found" };
            tracing::debug!(
                scope = scope.as_str(),
                episode_id = %episode_id,
                status,
                "memory.resolve keep",
            );
            Ok(ResolveResponse {
                status: status.to_string(),
                episode_id: episode_id.to_string(),
                invalidated: false,
                agenda_removed,
            })
        }

        ResolveAction::Invalidate => {
            scoped.forget(ForgetTarget::Id(episode_id)).await?;
            let agenda_removed = scoped.remove_verify_agenda(episode_id).await?;
            tracing::debug!(
                scope = scope.as_str(),
                episode_id = %episode_id,
                agenda_removed,
                "memory.resolve invalidate",
            );
            Ok(ResolveResponse {
                status: "invalidated".to_string(),
                episode_id: episode_id.to_string(),
                invalidated: true,
                agenda_removed,
            })
        }

        ResolveAction::Supersede => {
            let superseded_by = params.superseded_by.as_deref().ok_or_else(|| {
                ServiceError::InvalidInput(
                    "action=supersede requires superseded_by (the replacement episode's ULID)"
                        .to_string(),
                )
            })?;
            let _replacement: Ulid = superseded_by.parse().map_err(|e| {
                ServiceError::InvalidInput(format!(
                    "superseded_by is not a valid ULID: {e} (got {superseded_by:?})"
                ))
            })?;

            scoped.forget(ForgetTarget::Id(episode_id)).await?;
            let agenda_removed = scoped.remove_verify_agenda(episode_id).await?;
            tracing::debug!(
                scope = scope.as_str(),
                episode_id = %episode_id,
                superseded_by,
                agenda_removed,
                "memory.resolve supersede",
            );
            Ok(ResolveResponse {
                status: "superseded".to_string(),
                episode_id: episode_id.to_string(),
                invalidated: true,
                agenda_removed,
            })
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris::EpisodeBuilder;
    use lunaris_core::StubEmbedder;
    use lunaris_retrieve::Query;
    use std::sync::Arc;

    async fn make_engine() -> (Lunaris, Scope) {
        // `StubEmbedder` (deterministic 768-d vectors) so the recall-based
        // assertions below are reproducible on CI, which has no staged GGUF
        // embedder model — `Lunaris::open("memory://")` alone would fall back
        // to a NoopEmbedder whose zero vectors make recall return nothing
        // (mirrors `recall.rs`'s test harness).
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder)
            .await
            .expect("in-memory lunaris must open");
        let scope = Scope::new(format!("test.resolve-{}", Ulid::new())).unwrap();
        (lunaris, scope)
    }

    fn agenda_entry(episode_id: Ulid) -> lunaris::VerifyAgendaEntry {
        lunaris::VerifyAgendaEntry {
            episode_id,
            anchor_head: "a".repeat(40),
            current_head: "b".repeat(40),
            files: vec!["src/lib.rs".to_string()],
            first_seen_ms: 1_000,
            last_seen_ms: 1_000,
            v: 1,
        }
    }

    /// §2 "resolve keep prunes the agenda row, episode stays valid".
    #[tokio::test]
    async fn keep_prunes_agenda_row_episode_stays_live() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let id = Ulid::new();
        scoped
            .ingest(EpisodeBuilder::new("src:keep", "the violet antenna hums at dawn").id(id))
            .await
            .unwrap();
        scoped.upsert_verify_agenda(&[agenda_entry(id)]).await.unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            ResolveParams {
                episode_id: id.to_string(),
                action: ResolveAction::Keep,
                reason: None,
                superseded_by: None,
            },
        )
        .await
        .expect("resolve keep must succeed");

        assert_eq!(resp.status, "kept");
        assert!(!resp.invalidated, "keep must never invalidate");
        assert!(resp.agenda_removed, "keep must prune the agenda row");

        // The episode must still be live — a fresh recall surfaces it.
        let hits = scoped.recall(Query::text("violet antenna hum dawn")).await.unwrap();
        let bytes = id.to_bytes().to_vec();
        assert!(
            hits.iter().any(|h| h.episode_id == bytes),
            "kept episode must still recall: {hits:?}"
        );
    }

    /// §2 "resolve invalidate soft-deletes and prunes".
    #[tokio::test]
    async fn invalidate_soft_deletes_and_prunes() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let id = Ulid::new();
        scoped
            .ingest(EpisodeBuilder::new("src:inv", "the teal lighthouse rotates twice").id(id))
            .await
            .unwrap();
        scoped.upsert_verify_agenda(&[agenda_entry(id)]).await.unwrap();

        let hits_pre = scoped.recall(Query::text("teal lighthouse rotation")).await.unwrap();
        let bytes = id.to_bytes().to_vec();
        assert!(
            hits_pre.iter().any(|h| h.episode_id == bytes),
            "pre-resolve recall must surface the episode: {hits_pre:?}"
        );

        let resp = handle(
            &lunaris,
            &scope,
            ResolveParams {
                episode_id: id.to_string(),
                action: ResolveAction::Invalidate,
                reason: Some("stale after refactor".to_string()),
                superseded_by: None,
            },
        )
        .await
        .expect("resolve invalidate must succeed");

        assert_eq!(resp.status, "invalidated");
        assert!(resp.invalidated);
        assert!(resp.agenda_removed, "invalidate must prune the agenda row");

        // MVCC soft tombstone: read the raw episode row back and assert
        // bt.sys.1 (sys_to) is closed (non-null) — mirrors
        // forget_smoke.rs's B-5 assertion style.
        let key = lunaris_core::keyspace::episode_key(&scope, id);
        let read_at = lunaris.clock().tick();
        let row = lunaris.storage().read_as_of(&scope, &key, read_at).await.unwrap();
        let row = row.expect("soft-deleted episode row must still be present (not hard-deleted)");
        let json: serde_json::Value = serde_json::from_slice(&row.value).unwrap();
        let sys_to =
            json.get("bt").and_then(|bt| bt.get("sys")).and_then(|sys| sys.get(1)).cloned();
        assert!(
            sys_to.is_some_and(|v| !v.is_null()),
            "invalidate must close bt.sys[1] (soft tombstone); payload={json}"
        );

        // A fresh recall must no longer surface the invalidated episode.
        let hits_post = scoped.recall(Query::text("teal lighthouse rotation")).await.unwrap();
        assert!(
            !hits_post.iter().any(|h| h.episode_id == bytes),
            "post-invalidate recall must NOT surface the tombstoned episode: {hits_post:?}"
        );
    }

    /// §2 "resolve supersede requires superseded_by".
    #[tokio::test]
    async fn supersede_without_superseded_by_is_invalid_input() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let id = Ulid::new();
        scoped.ingest(EpisodeBuilder::new("src:sup", "note").id(id)).await.unwrap();
        scoped.upsert_verify_agenda(&[agenda_entry(id)]).await.unwrap();

        let err = handle(
            &lunaris,
            &scope,
            ResolveParams {
                episode_id: id.to_string(),
                action: ResolveAction::Supersede,
                reason: None,
                superseded_by: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)), "expected InvalidInput, got {err:?}");

        // Nothing must have been invalidated or removed.
        let key = lunaris_core::keyspace::episode_key(&scope, id);
        let read_at = lunaris.clock().tick();
        let row = lunaris.storage().read_as_of(&scope, &key, read_at).await.unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&row.value).unwrap();
        let sys_to =
            json.get("bt").and_then(|bt| bt.get("sys")).and_then(|sys| sys.get(1)).cloned();
        assert!(
            sys_to.is_none_or(|v| v.is_null()),
            "rejected supersede must NOT touch the episode's bt.sys[1]"
        );
        let listed = scoped.list_verify_agenda().await.unwrap();
        assert!(
            listed.iter().any(|e| e.episode_id == id),
            "rejected supersede must NOT prune the agenda row"
        );
    }

    /// §2 "resolve supersede with replacement id".
    #[tokio::test]
    async fn supersede_with_replacement_id_tombstones_and_echoes() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let id = Ulid::new();
        let replacement = Ulid::new();
        scoped.ingest(EpisodeBuilder::new("src:sup2", "the old note").id(id)).await.unwrap();
        scoped.upsert_verify_agenda(&[agenda_entry(id)]).await.unwrap();

        let resp = handle(
            &lunaris,
            &scope,
            ResolveParams {
                episode_id: id.to_string(),
                action: ResolveAction::Supersede,
                reason: None,
                superseded_by: Some(replacement.to_string()),
            },
        )
        .await
        .expect("resolve supersede with a valid replacement must succeed");

        assert_eq!(resp.status, "superseded");
        assert!(resp.invalidated);
        assert!(resp.agenda_removed);

        let key = lunaris_core::keyspace::episode_key(&scope, id);
        let read_at = lunaris.clock().tick();
        let row = lunaris.storage().read_as_of(&scope, &key, read_at).await.unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&row.value).unwrap();
        let sys_to =
            json.get("bt").and_then(|bt| bt.get("sys")).and_then(|sys| sys.get(1)).cloned();
        assert!(
            sys_to.is_some_and(|v| !v.is_null()),
            "supersede must close bt.sys[1] (soft tombstone) exactly like invalidate"
        );
    }

    /// §2 "resolve keep on an absent agenda row".
    #[tokio::test]
    async fn keep_on_absent_agenda_row_is_not_found() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let id = Ulid::new();
        scoped.ingest(EpisodeBuilder::new("src:no-agenda", "note").id(id)).await.unwrap();
        // Deliberately do NOT seed a verify_agenda row for `id`.

        let resp = handle(
            &lunaris,
            &scope,
            ResolveParams {
                episode_id: id.to_string(),
                action: ResolveAction::Keep,
                reason: None,
                superseded_by: None,
            },
        )
        .await
        .expect("resolve keep on an absent agenda row must not error");

        assert_eq!(resp.status, "not_found");
        assert!(!resp.invalidated);
        assert!(!resp.agenda_removed, "no agenda row existed to prune");
    }

    /// §2 "resolve on a malformed episode_id".
    #[tokio::test]
    async fn malformed_episode_id_is_invalid_input() {
        let (lunaris, scope) = make_engine().await;

        let err = handle(
            &lunaris,
            &scope,
            ResolveParams {
                episode_id: "not-a-ulid".to_string(),
                action: ResolveAction::Keep,
                reason: None,
                superseded_by: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)), "expected InvalidInput, got {err:?}");
    }

    /// `deny_unknown_fields` rejects a smuggled `scope` field.
    #[test]
    fn params_reject_smuggled_scope() {
        let raw = serde_json::json!({
            "episode_id": "01HZZZZZZZZZZZZZZZZZZZZZZZ",
            "action": "keep",
            "scope": "other",
        });
        let parsed: Result<ResolveParams, _> = serde_json::from_value(raw);
        assert!(parsed.is_err(), "deny_unknown_fields must reject a smuggled scope");
    }

    /// `ResolveAction` serializes snake_case per the frozen contract.
    #[test]
    fn action_serializes_snake_case() {
        assert_eq!(serde_json::to_value(ResolveAction::Keep).unwrap(), serde_json::json!("keep"));
        assert_eq!(
            serde_json::to_value(ResolveAction::Supersede).unwrap(),
            serde_json::json!("supersede")
        );
        assert_eq!(
            serde_json::to_value(ResolveAction::Invalidate).unwrap(),
            serde_json::json!("invalidate")
        );
    }
}
