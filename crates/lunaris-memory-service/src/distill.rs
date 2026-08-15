//! `memory.distill` — write a typed knowledge record and archive its source
//! episodes (activation drop, provenance preserved). engram-soul-loop
//! **task 8b** (`.add/tasks/distill/TASK.md` §3 CONTRACT, frozen; split from
//! milestone task 8, task 8a = `memory.dream_agenda`).
//!
//! The coding harness (the judge) reasons over a `memory.dream_agenda`
//! cluster and authors the distilled prose; this handler is the
//! TRANSACTIONAL APPLY step — Lunaris writes the harness's prose durably and
//! archives the raw sources it was distilled from.
//!
//! ## INGEST-04
//!
//! This handler MUST route the distilled episode through
//! `ScopedLunaris::ingest` (or `ScopedLunaris::ingest_idempotent`) and never
//! issue a second raw storage batch-commit call for it. Archiving the source
//! episodes rides `ScopedLunaris::archive_activation` — the SEPARATE ledger
//! RMW write path (mirrors `record_activation_refs`), not a second ingest.
//! A grep for the forbidden raw storage batch-commit primitive's name
//! (`StoragePort`'s single-call-per-ingest write method — see
//! `crates/lunaris/src/handle.rs`'s `ScopedLunaris::ingest` doc comment for
//! its exact name) against this file must return zero hits — this file
//! deliberately avoids spelling that name even in comments, so the
//! invariant holds trivially.
//!
//! ## Archive semantics — activation drop, NOT a tombstone
//!
//! Archiving a source episode never touches the episode itself (no
//! `forget`/soft-delete). It only stamps the source's activation-ledger
//! record so that (a) `lunaris_retrieve::LedgerBoostProvider::priors` gives
//! it zero recall boost thereafter, and (b) `memory.dream_agenda`
//! (`lunaris_consolidate::dream::build_dream_agenda`) no longer lists it as
//! a candidate. The source episode stays fully recall-hydratable — its base
//! similarity score survives.
//!
//! ## Content MUST be plain text, never JSON
//!
//! `lunaris_hook::context::summarize_memory_for_context` drops any
//! unrecognized JSON envelope (the 2026-07-14 anti-injection policy). The
//! distilled `content` field is stored VERBATIM as the harness's plain
//! prose — never wrapped in a JSON envelope — or a distilled record would be
//! silently dropped from the SessionStart digest, defeating the feature.
//! `kind` / `source_episode_ids` / `tag_count` live in `metadata` only.
//!
//! The `scope` argument is the **only** partition key; the wire DTO
//! intentionally carries no `scope` or `tenant` field (CLAUDE.md DTO
//! discipline).

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::ServiceError;
use lunaris::{EpisodeBuilder, IngestKind, Lunaris};
use lunaris_core::Scope;

// ── Kind ─────────────────────────────────────────────────────────────────────

/// Kind of distilled knowledge record — closed set for v1.
///
/// `procedure` is RESERVED for a future ATG (agentic tool graph)
/// procedural-memory kind (MILESTONE.md line 44) — deliberately NOT a
/// variant here. `parse` rejects it (and any other unrecognized string)
/// exactly like every unlisted value, surfaced by [`handle`] as the frozen
/// `"invalid_kind"` reject code.
///
/// Modeled as a plain string on the wire (`DistillParams::kind: String`)
/// rather than a `#[serde(deny_unknown_fields)]`-style closed serde enum so
/// `handle` can perform this validation itself and return the EXACT frozen
/// reject code — a derived enum would instead fail JSON deserialization
/// with a generic serde message before `handle` ever runs, which cannot be
/// steered to the specific `"invalid_kind"` string the frozen §2 scenario
/// requires. Mirrors this same file's `content`/`source_episode_ids`
/// validation style (business checks inside `handle`, not wire-typed
/// newtypes) and `resolve.rs`'s `episode_id`/`superseded_by` precedent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistillKind {
    Decision,
    Lesson,
    Invariant,
    Gotcha,
}

impl DistillKind {
    fn as_str(self) -> &'static str {
        match self {
            DistillKind::Decision => "decision",
            DistillKind::Lesson => "lesson",
            DistillKind::Invariant => "invariant",
            DistillKind::Gotcha => "gotcha",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "decision" => Some(DistillKind::Decision),
            "lesson" => Some(DistillKind::Lesson),
            "invariant" => Some(DistillKind::Invariant),
            "gotcha" => Some(DistillKind::Gotcha),
            // "procedure" and every other string fall through to `None` —
            // reserved-but-rejected in v1 (see the type doc comment).
            _ => None,
        }
    }
}

impl std::fmt::Display for DistillKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.distill`.
///
/// `#[serde(deny_unknown_fields)]` is mandatory (CLAUDE.md §HTTP DTO
/// discipline). The scope field is absent by design — it is bound at server
/// startup and cannot be overridden by the wire payload.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DistillParams {
    /// One of `decision` | `lesson` | `invariant` | `gotcha` (snake_case).
    /// `procedure` is reserved for a future ATG kind and rejected in v1
    /// as `"invalid_kind"`.
    pub kind: String,

    /// The harness-authored distilled prose. Stored VERBATIM as plain text
    /// — never wrapped in JSON (see the module doc's anti-injection note).
    /// Empty or whitespace-only is rejected as `"empty_content"`.
    pub content: String,

    /// Provenance: the raw source episodes this record distills. Every
    /// entry must be a valid ULID string. Empty is rejected as
    /// `"empty_provenance"`; any non-ULID entry is rejected as
    /// `"invalid_source_id"`.
    pub source_episode_ids: Vec<String>,

    /// Optional human-readable title. Accepted for forward compatibility
    /// but NOT yet persisted in v1 — the frozen §3 CONTRACT meta shape
    /// carries only `kind` / `source_episode_ids` / `tag_count` (mirrors
    /// `resolve.rs`'s `reason` field, accepted-but-unpersisted in v1).
    #[serde(default)]
    pub title: Option<String>,

    /// Optional tags. Only `tags.len()` is persisted (as `meta.tag_count`,
    /// mirroring `record_decision.rs`'s identical convention) — the tag
    /// strings themselves are not part of the frozen §3 meta shape.
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    /// Optional dedupe key (HOOK-05). A replay with the same key returns the
    /// prior `distilled_episode_id`/`lsn`, `was_duplicate=true`, and does
    /// NOT re-archive the sources (`archived_count=0` on the duplicate).
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Output of a `memory.distill` call. FLAT root (CLAUDE.md MCP
/// response-schema invariant — never a `#[serde(tag)]` enum).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DistillResponse {
    /// `"ok"` on a fresh apply, `"duplicate"` on a dedupe replay.
    pub status: String,
    /// The distilled episode's ULID (string form).
    pub distilled_episode_id: String,
    /// Log-sequence number of the committed write (wall_ms:counter) — the
    /// PRIOR commit's LSN on a duplicate replay.
    pub lsn: String,
    /// Count of source records actually marked archived. `0` on a
    /// duplicate replay (already archived on the first apply).
    pub archived_count: usize,
    /// `true` iff this call returned a previously-committed dedupe hit.
    pub was_duplicate: bool,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.distill`.
///
/// Validates FIRST, in frozen §1 Reject order — a rejected request writes
/// no episode and archives no source:
/// 1. `source_episode_ids` empty → `"empty_provenance"`.
/// 2. any `source_episode_ids` entry not a ULID → `"invalid_source_id"`.
/// 3. `content` empty/whitespace → `"empty_content"`.
/// 4. unknown `kind` (incl. `"procedure"`) → `"invalid_kind"`.
///
/// Then writes ONE `distilled:{kind}:{scope}` episode (via `ingest` /
/// `ingest_idempotent`) and archives every source via
/// `ScopedLunaris::archive_activation` — skipped entirely on a dedupe
/// duplicate.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: DistillParams,
) -> Result<DistillResponse, ServiceError> {
    if params.source_episode_ids.is_empty() {
        return Err(ServiceError::InvalidInput("empty_provenance".to_string()));
    }
    let mut source_ulids: Vec<Ulid> = Vec::with_capacity(params.source_episode_ids.len());
    for raw in &params.source_episode_ids {
        let id = Ulid::from_string(raw)
            .map_err(|_| ServiceError::InvalidInput("invalid_source_id".to_string()))?;
        source_ulids.push(id);
    }
    if params.content.trim().is_empty() {
        return Err(ServiceError::InvalidInput("empty_content".to_string()));
    }
    let kind = DistillKind::parse(&params.kind)
        .ok_or_else(|| ServiceError::InvalidInput("invalid_kind".to_string()))?;

    let source = format!("distilled:{kind}:{}", scope.as_str());
    let tag_count = params.tags.as_ref().map_or(0, |v| v.len());

    let mut meta = serde_json::Map::new();
    meta.insert("kind".into(), serde_json::Value::String(kind.as_str().to_string()));
    meta.insert(
        "source_episode_ids".into(),
        serde_json::Value::Array(
            params.source_episode_ids.iter().cloned().map(serde_json::Value::String).collect(),
        ),
    );
    meta.insert("tag_count".into(), serde_json::Value::Number(tag_count.into()));

    let scoped = lunaris.scoped(scope.clone());

    if let Some(ref key) = params.dedupe_key {
        // Deterministic id (blake3-derived, mirrors `EntityId` /
        // `lunaris-ingest`'s RAPTOR node-id precedent) so a dedupe REPLAY
        // can report back the SAME `distilled_episode_id` the first apply
        // minted. `ingest_idempotent`'s `IngestKind::Duplicate` carries only
        // the prior `Lsn` — not the prior episode id — so a randomly-minted
        // id would be unrecoverable on replay.
        let id = distilled_id_for_dedupe(scope, key);
        let builder = EpisodeBuilder::new(source, params.content).id(id).metadata(meta);
        let (lsn, ingest_kind) = scoped.ingest_idempotent(builder, key).await?;
        let was_duplicate = matches!(ingest_kind, IngestKind::Duplicate(_));

        let archived_count = if was_duplicate {
            // Already archived on the first apply — a replay must never
            // re-archive (frozen §1 Must: idempotency).
            0
        } else {
            let now = unix_now();
            scoped.archive_activation(&source_ulids, now).await?
        };

        tracing::debug!(
            scope = scope.as_str(),
            lsn = %lsn,
            duplicate = was_duplicate,
            archived_count,
            "memory.distill committed (idempotent path)",
        );

        Ok(DistillResponse {
            status: if was_duplicate { "duplicate".to_string() } else { "ok".to_string() },
            distilled_episode_id: id.to_string(),
            lsn: lsn.to_string(),
            archived_count,
            was_duplicate,
        })
    } else {
        let id = Ulid::new();
        let builder = EpisodeBuilder::new(source, params.content).id(id).metadata(meta);
        let lsn = scoped.ingest(builder).await?;

        let now = unix_now();
        let archived_count = scoped.archive_activation(&source_ulids, now).await?;

        tracing::debug!(
            scope = scope.as_str(),
            lsn = %lsn,
            archived_count,
            "memory.distill committed",
        );

        Ok(DistillResponse {
            status: "ok".to_string(),
            distilled_episode_id: id.to_string(),
            lsn: lsn.to_string(),
            archived_count,
            was_duplicate: false,
        })
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Deterministic ULID derived from `(scope, dedupe_key)` — 16-byte
/// truncation of `blake3(scope || dedupe_key)`, same construction as
/// `lunaris_extract::EntityId` / `lunaris-ingest`'s RAPTOR community-node
/// id. A replay with the same `dedupe_key` recomputes the identical id
/// locally without an extra storage round trip.
fn distilled_id_for_dedupe(scope: &Scope, dedupe_key: &str) -> Ulid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"lunaris-distill-dedupe-v1");
    hasher.update(b"::");
    hasher.update(scope.as_str().as_bytes());
    hasher.update(b"::");
    hasher.update(dedupe_key.as_bytes());
    let hash = hasher.finalize();
    let bytes: [u8; 16] =
        hash.as_bytes()[..16].try_into().expect("blake3 produces 32 bytes; first 16 always fit");
    Ulid::from_bytes(bytes)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::StubEmbedder;
    use lunaris_core::activation::{Grain, RefSignal, Strength};
    use lunaris_core::keyspace::{activation_key, episode_key};
    use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};
    use std::sync::Arc;

    /// Ported off `memory://` (0.7.0 prerequisite) onto a harness-issued
    /// ephemeral Moon; falls back to `memory://` only where no Moon binary
    /// exists. `TestEngine` derefs to `Lunaris`, so the call sites below are
    /// unchanged — but the binding owns the Moon child and must outlive them.
    async fn make_engine() -> (TestEngine, Scope) {
        // `StubEmbedder` (deterministic 768-d vectors) — the harness default
        // embedder, restated here so the dim is visible next to the assertions
        // (mirrors `resolve.rs` / `dream_agenda.rs`'s test harness).
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = open_test_engine_with_embedder(embedder).await;
        let scope = Scope::new(format!("test.distill-{}", Ulid::new())).unwrap();
        (lunaris, scope)
    }

    /// Seed one raw source episode with a ledger reference (so it has an
    /// activation record to archive) — mirrors `dream_agenda.rs`'s
    /// `ingest_structured` + `record_activation_refs` production-wiring
    /// pattern, simplified to a plain `ingest` since distill doesn't need
    /// structured facts.
    async fn seed_source(lunaris: &Lunaris, scope: &Scope, text: &str) -> Ulid {
        let scoped = lunaris.scoped(scope.clone());
        let id = Ulid::new();
        scoped
            .ingest(EpisodeBuilder::new("lunaris:tool_call:post", text.to_string()).id(id))
            .await
            .expect("seed source episode");
        scoped
            .record_activation_refs(&[RefSignal {
                id,
                grain: Grain::Turn,
                strength: Strength::Weak,
            }])
            .await
            .expect("seed source activation ref");
        id
    }

    async fn key_count(lunaris: &Lunaris, scope: &Scope) -> usize {
        use futures::StreamExt;
        let prefix = lunaris_core::keyspace::scope_prefix(scope).into_bytes();
        let storage = lunaris.storage();
        let mut stream = storage.scan_range(scope, &prefix, None).await.unwrap();
        let mut n = 0usize;
        while stream.next().await.is_some() {
            n += 1;
        }
        n
    }

    /// §2 "distill writes a typed record and archives its sources."
    #[tokio::test]
    async fn distill_writes_typed_record_and_archives_sources() {
        let (lunaris, scope) = make_engine().await;
        let a = seed_source(&lunaris, &scope, "raw episode A").await;
        let b = seed_source(&lunaris, &scope, "raw episode B").await;

        let resp = handle(
            &lunaris,
            &scope,
            DistillParams {
                kind: "lesson".to_string(),
                content: "prefer X over Y because Z".to_string(),
                source_episode_ids: vec![a.to_string(), b.to_string()],
                title: None,
                tags: None,
                dedupe_key: None,
            },
        )
        .await
        .expect("distill must succeed");

        assert_eq!(resp.status, "ok");
        assert!(!resp.was_duplicate);
        assert_eq!(resp.archived_count, 2);

        let distilled_id = Ulid::from_string(&resp.distilled_episode_id).unwrap();
        let key = episode_key(&scope, distilled_id);
        let read_at = lunaris.clock().tick();
        let row = lunaris.storage().read_as_of(&scope, &key, read_at).await.unwrap().unwrap();
        let episode: lunaris_core::Episode = serde_json::from_slice(&row.value).unwrap();
        assert_eq!(episode.source, format!("distilled:lesson:{}", scope.as_str()));
        assert_eq!(
            episode.content, "prefer X over Y because Z",
            "content must be PLAIN text, not JSON"
        );
        assert_eq!(episode.metadata.get("kind").and_then(|v| v.as_str()), Some("lesson"));
        let stored_ids: Vec<String> = episode
            .metadata
            .get("source_episode_ids")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(stored_ids, vec![a.to_string(), b.to_string()]);

        for source_id in [a, b] {
            let rec: lunaris_core::activation::ActivationRecord = serde_json::from_slice(
                &lunaris
                    .storage()
                    .read_as_of(&scope, &activation_key(&scope, source_id), read_at)
                    .await
                    .unwrap()
                    .unwrap()
                    .value,
            )
            .unwrap();
            assert!(rec.is_archived(), "source {source_id} must be archived");
        }
    }

    /// §2 "source episodes NOT tombstoned — a read_as_of after distill still
    /// returns them." Archive = activation drop, not a forget/soft-delete.
    #[tokio::test]
    async fn source_episodes_are_not_tombstoned() {
        let (lunaris, scope) = make_engine().await;
        let a = seed_source(&lunaris, &scope, "raw episode A").await;

        handle(
            &lunaris,
            &scope,
            DistillParams {
                kind: "gotcha".to_string(),
                content: "watch out for this".to_string(),
                source_episode_ids: vec![a.to_string()],
                title: None,
                tags: None,
                dedupe_key: None,
            },
        )
        .await
        .expect("distill must succeed");

        let read_at = lunaris.clock().tick();
        let key = episode_key(&scope, a);
        let row = lunaris.storage().read_as_of(&scope, &key, read_at).await.unwrap();
        let row = row.expect("archived source episode row must still be present");
        let json: serde_json::Value = serde_json::from_slice(&row.value).unwrap();
        let sys_to =
            json.get("bt").and_then(|bt| bt.get("sys")).and_then(|sys| sys.get(1)).cloned();
        assert!(
            sys_to.is_none_or(|v| v.is_null()),
            "archive must NOT close bt.sys[1] — it is not a tombstone; payload={json}"
        );

        // Also recallable via a fresh recall pass on the same scope.
        let hits = lunaris
            .scoped(scope.clone())
            .recall(lunaris_retrieve::Query::text("raw episode A"))
            .await
            .unwrap();
        let bytes = a.to_bytes().to_vec();
        assert!(
            hits.iter().any(|h| h.episode_id == bytes),
            "archived source must stay recallable: {hits:?}"
        );
    }

    /// §2 "idempotent replay does not re-archive."
    #[tokio::test]
    async fn idempotent_replay_returns_prior_id_and_does_not_rearchive() {
        let (lunaris, scope) = make_engine().await;
        let a = seed_source(&lunaris, &scope, "raw episode A").await;

        let params = |dedupe: &str| DistillParams {
            kind: "invariant".to_string(),
            content: "the cache must never outlive the connection".to_string(),
            source_episode_ids: vec![a.to_string()],
            title: None,
            tags: None,
            dedupe_key: Some(dedupe.to_string()),
        };

        let first = handle(&lunaris, &scope, params("distill-dedupe-1"))
            .await
            .expect("first distill must succeed");
        assert_eq!(first.status, "ok");
        assert!(!first.was_duplicate);
        assert_eq!(first.archived_count, 1);

        let replay = handle(&lunaris, &scope, params("distill-dedupe-1"))
            .await
            .expect("replay distill must succeed");
        assert_eq!(replay.status, "duplicate");
        assert!(replay.was_duplicate);
        assert_eq!(replay.archived_count, 0, "a duplicate must NOT re-archive");
        assert_eq!(
            replay.distilled_episode_id, first.distilled_episode_id,
            "a dedupe replay must return the SAME distilled_episode_id"
        );
        assert_eq!(replay.lsn, first.lsn, "a dedupe replay must return the prior LSN");
    }

    /// §2 reject matrix — each rejection writes NOTHING and archives NOTHING.
    #[tokio::test]
    async fn reject_empty_provenance() {
        let (lunaris, scope) = make_engine().await;
        let before = key_count(&lunaris, &scope).await;
        let err = handle(
            &lunaris,
            &scope,
            DistillParams {
                kind: "lesson".to_string(),
                content: "x".to_string(),
                source_episode_ids: vec![],
                title: None,
                tags: None,
                dedupe_key: None,
            },
        )
        .await
        .unwrap_err();
        match err {
            ServiceError::InvalidInput(msg) => assert_eq!(msg, "empty_provenance"),
            other => panic!("expected InvalidInput(empty_provenance), got {other:?}"),
        }
        assert_eq!(key_count(&lunaris, &scope).await, before, "rejection must write nothing");
    }

    #[tokio::test]
    async fn reject_invalid_source_id() {
        let (lunaris, scope) = make_engine().await;
        let before = key_count(&lunaris, &scope).await;
        let err = handle(
            &lunaris,
            &scope,
            DistillParams {
                kind: "lesson".to_string(),
                content: "x".to_string(),
                source_episode_ids: vec!["not-a-ulid".to_string()],
                title: None,
                tags: None,
                dedupe_key: None,
            },
        )
        .await
        .unwrap_err();
        match err {
            ServiceError::InvalidInput(msg) => assert_eq!(msg, "invalid_source_id"),
            other => panic!("expected InvalidInput(invalid_source_id), got {other:?}"),
        }
        assert_eq!(key_count(&lunaris, &scope).await, before, "rejection must write nothing");
    }

    #[tokio::test]
    async fn reject_empty_content() {
        let (lunaris, scope) = make_engine().await;
        let a = seed_source(&lunaris, &scope, "raw episode A").await;
        let before = key_count(&lunaris, &scope).await;
        let err = handle(
            &lunaris,
            &scope,
            DistillParams {
                kind: "lesson".to_string(),
                content: "   ".to_string(),
                source_episode_ids: vec![a.to_string()],
                title: None,
                tags: None,
                dedupe_key: None,
            },
        )
        .await
        .unwrap_err();
        match err {
            ServiceError::InvalidInput(msg) => assert_eq!(msg, "empty_content"),
            other => panic!("expected InvalidInput(empty_content), got {other:?}"),
        }
        assert_eq!(key_count(&lunaris, &scope).await, before, "rejection must write nothing");

        // The source's activation record must be untouched (not archived).
        let read_at = lunaris.clock().tick();
        let rec: lunaris_core::activation::ActivationRecord = serde_json::from_slice(
            &lunaris
                .storage()
                .read_as_of(&scope, &activation_key(&scope, a), read_at)
                .await
                .unwrap()
                .unwrap()
                .value,
        )
        .unwrap();
        assert!(!rec.is_archived(), "rejection must not archive the source");
    }

    #[tokio::test]
    async fn reject_invalid_kind_including_reserved_procedure() {
        let (lunaris, scope) = make_engine().await;
        let a = seed_source(&lunaris, &scope, "raw episode A").await;
        let before = key_count(&lunaris, &scope).await;

        for bad_kind in ["procedure", "nonsense", ""] {
            let err = handle(
                &lunaris,
                &scope,
                DistillParams {
                    kind: bad_kind.to_string(),
                    content: "x".to_string(),
                    source_episode_ids: vec![a.to_string()],
                    title: None,
                    tags: None,
                    dedupe_key: None,
                },
            )
            .await
            .unwrap_err();
            match err {
                ServiceError::InvalidInput(msg) => assert_eq!(msg, "invalid_kind"),
                other => {
                    panic!("expected InvalidInput(invalid_kind) for {bad_kind:?}, got {other:?}")
                }
            }
        }
        assert_eq!(key_count(&lunaris, &scope).await, before, "rejection must write nothing");
    }

    /// `deny_unknown_fields` rejects a smuggled `scope` field.
    #[test]
    fn params_reject_smuggled_scope() {
        let raw = serde_json::json!({
            "kind": "lesson",
            "content": "x",
            "source_episode_ids": ["01HZZZZZZZZZZZZZZZZZZZZZZZ"],
            "scope": "other",
        });
        let parsed: Result<DistillParams, _> = serde_json::from_value(raw);
        assert!(parsed.is_err(), "deny_unknown_fields must reject a smuggled scope");
    }

    /// `DistillKind::parse` accepts exactly the four v1 kinds and rejects
    /// everything else, including the reserved `"procedure"`.
    #[test]
    fn distill_kind_parse_matrix() {
        assert_eq!(DistillKind::parse("decision"), Some(DistillKind::Decision));
        assert_eq!(DistillKind::parse("lesson"), Some(DistillKind::Lesson));
        assert_eq!(DistillKind::parse("invariant"), Some(DistillKind::Invariant));
        assert_eq!(DistillKind::parse("gotcha"), Some(DistillKind::Gotcha));
        assert_eq!(DistillKind::parse("procedure"), None);
        assert_eq!(DistillKind::parse("Decision"), None, "kind must be exact snake_case");
    }
}
