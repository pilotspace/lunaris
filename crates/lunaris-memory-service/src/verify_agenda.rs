//! `memory.verify_agenda` — list memories flagged stale by the background
//! verify/staleness sweep (engram-soul-loop task 6's KV) with enough diff
//! context (anchor/current git heads, changed files) and a content snippet
//! for a human or harness to judge staleness.
//!
//! Consumes `ScopedLunaris::list_verify_agenda` (task 7) and hydrates each
//! entry's `snippet` from the referenced episode's `content` — a gone
//! episode still lists with `snippet == ""` (fail-open, so a stale agenda
//! row for a since-forgotten episode can still be `keep`-pruned via
//! `memory.resolve`).
//!
//! The `scope` argument is the **only** partition key; the wire DTO
//! intentionally carries no `scope` or `tenant` field (CLAUDE.md DTO
//! discipline).

use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;

/// Default `limit` when the caller omits it — matches `.add/tasks/
/// verify-agenda-tools/TASK.md` §3 CONTRACT.
const DEFAULT_LIMIT: usize = 100;

/// Snippet truncation cap (characters, not bytes — `char_indices` safe for
/// multi-byte UTF-8).
const SNIPPET_MAX_CHARS: usize = 280;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.verify_agenda`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifyAgendaParams {
    /// Maximum number of agenda items to return. Defaults to 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One stale-flagged memory, with diff context and a content snippet.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgendaItem {
    /// The stale-anchored episode's ULID (string form).
    pub episode_id: String,
    /// Git HEAD the episode was anchored to when it was recorded.
    pub anchor_head: String,
    /// The current git HEAD it now disagrees with.
    pub current_head: String,
    /// Files whose diff between `anchor_head` and `current_head` triggered
    /// the staleness flag.
    pub files: Vec<String>,
    /// Epoch-ms the agenda row was first created for this episode.
    pub first_seen_ms: u64,
    /// Epoch-ms the agenda row was last (re-)upserted.
    pub last_seen_ms: u64,
    /// The episode's content, trimmed to 280 chars. Empty string if the
    /// episode has since been removed (fail-open — the row still lists so
    /// it can be `keep`-pruned).
    pub snippet: String,
}

/// Output of a successful `memory.verify_agenda` call. FLAT root (CLAUDE.md
/// MCP response-schema invariant — never a `#[serde(tag)]` enum).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VerifyAgendaResponse {
    /// Number of items in `items` (after the `limit` truncation).
    pub count: usize,
    /// Agenda items, freshest staleness (`last_seen_ms` DESC) first.
    pub items: Vec<AgendaItem>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.verify_agenda`.
///
/// 1. `ScopedLunaris::list_verify_agenda` — scan + skip-corrupt + sort
///    `last_seen_ms` DESC (task 7 engine helper).
/// 2. Truncate to `limit` (default 100).
/// 3. Hydrate each entry's `snippet` via `read_as_of` on the episode key —
///    a miss or corrupt row yields `snippet == ""`, never an error (fail
///    open per `.add/tasks/verify-agenda-tools/TASK.md` §1 Reject).
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: VerifyAgendaParams,
) -> Result<VerifyAgendaResponse, ServiceError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let scoped = lunaris.scoped(scope.clone());
    let mut entries = scoped.list_verify_agenda().await?;
    entries.truncate(limit);

    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let snippet = hydrate_snippet(lunaris, scope, entry.episode_id).await;
        items.push(AgendaItem {
            episode_id: entry.episode_id.to_string(),
            anchor_head: entry.anchor_head,
            current_head: entry.current_head,
            files: entry.files,
            first_seen_ms: entry.first_seen_ms,
            last_seen_ms: entry.last_seen_ms,
            snippet,
        });
    }

    tracing::debug!(scope = scope.as_str(), count = items.len(), "memory.verify_agenda listed",);

    Ok(VerifyAgendaResponse { count: items.len(), items })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Hydrate one agenda entry's content snippet. Fail-open to `""` on any
/// miss, storage error, or corrupt payload — a gone episode must still list
/// (`.add/tasks/verify-agenda-tools/TASK.md` §1 Must).
async fn hydrate_snippet(lunaris: &Lunaris, scope: &Scope, episode_id: ulid::Ulid) -> String {
    let key = lunaris_core::keyspace::episode_key(scope, episode_id);
    let read_at = lunaris.clock().tick();
    let row = match lunaris.storage().read_as_of(scope, &key, read_at).await {
        Ok(Some(row)) => row,
        Ok(None) => return String::new(),
        Err(e) => {
            tracing::debug!(
                err = %e,
                episode_id = %episode_id,
                scope = scope.as_str(),
                "verify_agenda_snippet_hydrate_failed"
            );
            return String::new();
        }
    };
    match serde_json::from_slice::<lunaris_core::Episode>(&row.value) {
        Ok(ep) => truncate_snippet(&ep.content, SNIPPET_MAX_CHARS),
        Err(_) => String::new(),
    }
}

/// Truncate `content` to at most `max_chars` UTF-8 chars (never bytes — a
/// byte-cap could split a multi-byte codepoint).
fn truncate_snippet(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        content.to_string()
    } else {
        content.chars().take(max_chars).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris::EpisodeBuilder;
    use ulid::Ulid;

    async fn make_engine() -> (Lunaris, Scope) {
        let lunaris = Lunaris::open("memory://").await.expect("in-memory lunaris must open");
        let scope = Scope::new(format!("test.verify-agenda-{}", Ulid::new())).unwrap();
        (lunaris, scope)
    }

    fn agenda_entry(
        episode_id: Ulid,
        anchor_head: &str,
        current_head: &str,
        files: &[&str],
        first_seen_ms: u64,
        last_seen_ms: u64,
    ) -> lunaris::VerifyAgendaEntry {
        lunaris::VerifyAgendaEntry {
            episode_id,
            anchor_head: anchor_head.to_owned(),
            current_head: current_head.to_owned(),
            files: files.iter().map(|s| s.to_string()).collect(),
            first_seen_ms,
            last_seen_ms,
            v: 1,
        }
    }

    /// §2 "verify_agenda lists stale entries with snippet + diff context":
    /// two seeded entries (episodes present) -> count == 2, both carry
    /// episode_id/files/anchor_head/current_head + a non-empty snippet,
    /// ordered by last_seen_ms DESC.
    #[tokio::test]
    async fn lists_stale_entries_with_snippet_and_diff_context() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let id_a = Ulid::new();
        let id_b = Ulid::new();
        scoped
            .ingest(EpisodeBuilder::new("src:a", "the amber relay hums at dawn").id(id_a))
            .await
            .unwrap();
        scoped
            .ingest(EpisodeBuilder::new("src:b", "the cobalt beacon flashes twice").id(id_b))
            .await
            .unwrap();

        scoped
            .upsert_verify_agenda(&[
                agenda_entry(id_a, &"a".repeat(40), &"b".repeat(40), &["src/a.rs"], 1_000, 1_000),
                agenda_entry(id_b, &"a".repeat(40), &"b".repeat(40), &["src/b.rs"], 2_000, 5_000),
            ])
            .await
            .unwrap();

        let resp = handle(&lunaris, &scope, VerifyAgendaParams { limit: None })
            .await
            .expect("verify_agenda must succeed");

        assert_eq!(resp.count, 2, "both seeded agenda entries must list");
        assert_eq!(resp.items.len(), 2);
        assert_eq!(
            resp.items[0].episode_id,
            id_b.to_string(),
            "last_seen_ms DESC: b (5_000) first"
        );
        assert_eq!(
            resp.items[1].episode_id,
            id_a.to_string(),
            "last_seen_ms DESC: a (1_000) second"
        );
        for item in &resp.items {
            assert!(!item.files.is_empty(), "files must carry the diff context");
            assert_eq!(item.anchor_head, "a".repeat(40));
            assert_eq!(item.current_head, "b".repeat(40));
            assert!(!item.snippet.is_empty(), "a live episode must yield a non-empty snippet");
        }
    }

    /// §2 "verify_agenda lists an entry whose episode is gone (empty
    /// snippet)": the episode row is hard-removed out from under the
    /// agenda entry -> the entry still lists with snippet == "".
    #[tokio::test]
    async fn lists_entry_with_gone_episode_as_empty_snippet() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        // Deliberately do NOT ingest an episode for this id — simulates a
        // gone/never-materialized episode behind the agenda row.
        let ghost_id = Ulid::new();
        scoped
            .upsert_verify_agenda(&[agenda_entry(
                ghost_id,
                &"a".repeat(40),
                &"b".repeat(40),
                &["src/ghost.rs"],
                1_000,
                1_000,
            )])
            .await
            .unwrap();

        let resp = handle(&lunaris, &scope, VerifyAgendaParams { limit: None })
            .await
            .expect("verify_agenda must succeed even with a gone episode");

        assert_eq!(resp.count, 1, "the agenda row for the gone episode must still list");
        assert_eq!(resp.items[0].episode_id, ghost_id.to_string());
        assert_eq!(resp.items[0].snippet, "", "a gone episode must yield an empty snippet");
    }

    /// `limit` truncates the returned items (but not the reported diff
    /// context per item).
    #[tokio::test]
    async fn limit_truncates_returned_items() {
        let (lunaris, scope) = make_engine().await;
        let scoped = lunaris.scoped(scope.clone());

        let mut entries = Vec::new();
        for i in 0..5u64 {
            let id = Ulid::new();
            scoped
                .ingest(EpisodeBuilder::new(format!("src:{i}"), format!("note {i}")).id(id))
                .await
                .unwrap();
            entries.push(agenda_entry(
                id,
                &"a".repeat(40),
                &"b".repeat(40),
                &["src/f.rs"],
                1_000,
                1_000 + i,
            ));
        }
        scoped.upsert_verify_agenda(&entries).await.unwrap();

        let resp = handle(&lunaris, &scope, VerifyAgendaParams { limit: Some(2) })
            .await
            .expect("verify_agenda must succeed");

        assert_eq!(resp.count, 2, "limit=2 must cap the returned items to 2");
        assert_eq!(resp.items.len(), 2);
    }

    /// `deny_unknown_fields` rejects a smuggled `scope` field.
    #[test]
    fn params_reject_smuggled_scope() {
        let raw = serde_json::json!({"limit": 5, "scope": "other"});
        let parsed: Result<VerifyAgendaParams, _> = serde_json::from_value(raw);
        assert!(parsed.is_err(), "deny_unknown_fields must reject a smuggled scope");
    }
}
