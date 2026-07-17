//! engram-soul-loop task 6 (staleness-pass) — pure staleness assessment +
//! the shared verify-agenda sweep.
//!
//! `.add/tasks/staleness-pass/TASK.md` §3 CONTRACT:
//! - [`assess`] is PURE given a changed-files closure: stale iff
//!   `meta.git_head` is a 40-hex string that differs from `current_head`
//!   AND `meta.files` is a non-empty array intersecting
//!   `changed(anchor_head)`. `None` from the closure (a git/read failure)
//!   fails OPEN to "not stale" — a staleness verdict must never surface a
//!   git failure to the recall/digest path (§1 Reject).
//! - [`sweep_and_upsert`] is the shared digest-arm + commit-capture-arm
//!   sweep: scans the scope's episode partition (bounded, warn-and-partial
//!   past `SCAN_CAP`), resolves at most
//!   [`crate::git_anchor::MAX_ANCHOR_DIFFS_PER_SWEEP`] distinct anchor
//!   diffs, and upserts stale entries via
//!   `ScopedLunaris::upsert_verify_agenda` in ONE atomic batch. Every
//!   failure path degrades to "no sweep" — never surfaces as an `Err` to
//!   the digest/capture caller (§1 Reject).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use lunaris::{Lunaris, VerifyAgendaEntry};
use lunaris_core::{Scope, keyspace};
use serde_json::{Map, Value};

/// Post-curation score decay applied to a stale hit (§1 Must). A stale hit
/// is never demoted below the curation cut in v1 (recorded spec delta —
/// hydrate-time metadata threading is the follow-up); the decay + banner
/// are the primary signal.
pub(crate) const STALE_DECAY: f32 = 0.7;

/// Hard cap on episode rows scanned per sweep call (handover.rs SCAN_CAP
/// precedent) — a DoS guard for huge scopes; excess is a warn-and-partial
/// sweep, never a hard failure.
const SCAN_CAP: usize = 5_000;

/// Outcome of [`assess`] for a single curated memory / swept episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StaleVerdict {
    pub stale: bool,
}

/// Pure staleness assessment given a memory's stored `meta` (an Episode's
/// `metadata` map), the resolved `current_head`, and a synchronous
/// changed-files lookup (`anchor_head -> Option<changed file set>`, `None`
/// meaning "unresolvable — fail open").
///
/// Stale iff:
/// - `meta.git_head` is present, a 40-hex string, AND differs from
///   `current_head` (equal heads, or a malformed/absent value, are NEVER
///   stale), AND
/// - `meta.files` is present and a NON-EMPTY array, AND
/// - `changed(anchor_head)` returns `Some(set)` that intersects
///   `meta.files` (a `None` — unresolvable diff — fails open to fresh; an
///   empty or non-overlapping set is also fresh — §1 Reject: "marking
///   stale on HEAD-mismatch alone (no files overlap) -> forbidden").
pub(crate) fn assess(
    meta: &Map<String, Value>,
    current_head: &str,
    changed: &dyn Fn(&str) -> Option<HashSet<String>>,
) -> StaleVerdict {
    let fresh = StaleVerdict { stale: false };

    let Some(anchor_head) = meta.get("git_head").and_then(Value::as_str) else {
        return fresh;
    };
    let is_40_hex = anchor_head.len() == 40 && anchor_head.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_40_hex || anchor_head == current_head {
        return fresh;
    }

    let Some(files) = meta.get("files").and_then(Value::as_array) else {
        return fresh;
    };
    if files.is_empty() {
        return fresh;
    }
    let anchored_files: HashSet<&str> = files.iter().filter_map(Value::as_str).collect();
    if anchored_files.is_empty() {
        return fresh;
    }

    match changed(anchor_head) {
        Some(changed_files) => {
            let stale = anchored_files.iter().any(|f| changed_files.contains(*f));
            StaleVerdict { stale }
        }
        None => fresh,
    }
}

/// Shared verify-agenda sweep: digest arm + commit-capture arm both call
/// this with the same signature. Fire-and-forget by construction — callers
/// MUST wrap this in `tokio::spawn` (mirrors `spawn_capture_tool`); this
/// function itself never returns an error to a turn/digest path.
///
/// Scans `scope`'s `episode:` partition (bounded `SCAN_CAP`,
/// warn-and-partial past the cap), keeps rows carrying a `git_head` meta,
/// resolves at most [`crate::git_anchor::MAX_ANCHOR_DIFFS_PER_SWEEP`]
/// DISTINCT anchor diffs (excess anchors are skipped fail-open — caught by
/// the next sweep), assesses each anchored episode, and upserts the STALE
/// ones via `ScopedLunaris::upsert_verify_agenda` in one atomic batch.
pub(crate) async fn sweep_and_upsert(handle: &Lunaris, scope: &Scope, cwd: &Path) {
    let Some(current_head) = crate::git_anchor::head_for_cwd(cwd).await else {
        // No resolvable HEAD (non-repo cwd, git failure) — nothing to sweep.
        return;
    };

    let storage = handle.storage();
    let prefix = keyspace::episode_prefix(scope);
    let mut stream = match storage.scan_range(scope, &prefix, None).await {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(err = %err, "verify_agenda sweep: episode scan failed");
            return;
        }
    };

    // (episode_id, anchor_head, files) for every episode carrying a
    // git_head meta — regardless of whether it turns out stale.
    let mut anchored: Vec<(ulid::Ulid, String, Vec<String>)> = Vec::new();
    let mut scanned = 0usize;
    while let Some(item) = stream.next().await {
        let (_key, value) = match item {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        scanned += 1;
        if let Ok(ep) = serde_json::from_slice::<lunaris_core::Episode>(&value)
            && let Some(head) = ep.metadata.get("git_head").and_then(Value::as_str)
        {
            let files: Vec<String> = ep
                .metadata
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            anchored.push((ep.id, head.to_owned(), files));
        }
        if scanned >= SCAN_CAP {
            tracing::warn!(
                scanned,
                scope = scope.as_str(),
                "verify_agenda sweep: episode scan cap reached — partial sweep"
            );
            break;
        }
    }
    if anchored.is_empty() {
        return;
    }

    // Resolve at most MAX_ANCHOR_DIFFS_PER_SWEEP DISTINCT anchor heads
    // (excluding heads already equal to current — never stale, no diff
    // needed). Excess distinct anchors are simply not in `diffs` below,
    // which `assess` treats as a closure-None fail-open (caught next sweep).
    let mut distinct_heads: Vec<String> = Vec::new();
    for (_, head, _) in &anchored {
        if head != &current_head && !distinct_heads.contains(head) {
            distinct_heads.push(head.clone());
        }
        if distinct_heads.len() >= crate::git_anchor::MAX_ANCHOR_DIFFS_PER_SWEEP {
            break;
        }
    }
    let mut diffs: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    for head in &distinct_heads {
        let resolved = crate::git_anchor::changed_files_since(cwd, head).await;
        diffs.insert(head.clone(), resolved);
    }

    let now_ms =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);

    let mut entries: Vec<VerifyAgendaEntry> = Vec::new();
    for (episode_id, anchor_head, files) in anchored {
        let mut meta = Map::new();
        meta.insert("git_head".into(), Value::String(anchor_head.clone()));
        meta.insert(
            "files".into(),
            Value::Array(files.iter().cloned().map(Value::String).collect()),
        );

        let changed_lookup = |head: &str| diffs.get(head).cloned().flatten();
        let verdict = assess(&meta, &current_head, &changed_lookup);
        if !verdict.stale {
            continue;
        }

        let overlap: Vec<String> = diffs
            .get(&anchor_head)
            .cloned()
            .flatten()
            .map(|changed_set| files.iter().filter(|f| changed_set.contains(*f)).cloned().collect())
            .unwrap_or_default();

        entries.push(VerifyAgendaEntry {
            episode_id,
            anchor_head,
            current_head: current_head.clone(),
            files: overlap,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
            v: 1,
        });
    }

    if entries.is_empty() {
        return;
    }
    if let Err(err) = handle.scoped(scope.clone()).upsert_verify_agenda(&entries).await {
        tracing::warn!(err = %err, scope = scope.as_str(), "verify_agenda sweep: upsert failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_with(
        git_head: Option<&str>,
        files: Option<&[&str]>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        if let Some(head) = git_head {
            m.insert("git_head".into(), serde_json::Value::String(head.to_owned()));
        }
        if let Some(files) = files {
            m.insert(
                "files".into(),
                serde_json::Value::Array(
                    files.iter().map(|f| serde_json::Value::String((*f).to_owned())).collect(),
                ),
            );
        }
        m
    }

    fn hex40(byte: char) -> String {
        byte.to_string().repeat(40)
    }

    /// Truth-table case 1: valid differing anchor + overlapping changed set
    /// -> stale.
    #[test]
    fn assess_stale_when_anchor_differs_and_files_overlap() {
        let anchor = hex40('a');
        let current = hex40('b');
        let meta = meta_with(Some(&anchor), Some(&["src/lib.rs"]));
        let changed = |h: &str| -> Option<HashSet<String>> {
            assert_eq!(h, anchor);
            Some(HashSet::from(["src/lib.rs".to_owned()]))
        };
        let verdict = super::assess(&meta, &current, &changed);
        assert!(verdict.stale, "anchored file touched since anchor_head must be stale");
    }

    /// Truth-table case 2: valid differing anchor but the changed set does
    /// NOT include the anchored file -> fresh (§1 Reject: no HEAD-mismatch-
    /// alone staleness).
    #[test]
    fn assess_fresh_when_anchored_file_untouched() {
        let anchor = hex40('a');
        let current = hex40('b');
        let meta = meta_with(Some(&anchor), Some(&["src/lib.rs"]));
        let changed =
            |_: &str| -> Option<HashSet<String>> { Some(HashSet::from(["README.md".to_owned()])) };
        let verdict = super::assess(&meta, &current, &changed);
        assert!(!verdict.stale, "an untouched anchored file must stay fresh");
    }

    /// Truth-table case 3: no `git_head` meta key at all -> fresh, and the
    /// closure must never even be consulted (no anchor => nothing to diff).
    #[test]
    fn assess_fresh_when_unanchored() {
        let meta = meta_with(None, Some(&["src/lib.rs"]));
        let changed = |_: &str| -> Option<HashSet<String>> {
            panic!("changed() must not be called for an unanchored memory")
        };
        let verdict = super::assess(&meta, &hex40('b'), &changed);
        assert!(!verdict.stale, "an unanchored memory must never be stale");
    }

    /// Truth-table case 4: the changed-files closure fails open (`None` —
    /// e.g. a git subprocess failure) -> fresh, never stale.
    #[test]
    fn assess_fresh_when_changed_lookup_fails_open() {
        let anchor = hex40('a');
        let current = hex40('b');
        let meta = meta_with(Some(&anchor), Some(&["src/lib.rs"]));
        let changed = |_: &str| -> Option<HashSet<String>> { None };
        let verdict = super::assess(&meta, &current, &changed);
        assert!(!verdict.stale, "a closure-None (git failure) must fail open to fresh");
    }

    /// Extra case: anchor_head equal to current_head -> fresh, closure must
    /// not even be consulted (no diff needed when nothing has moved).
    #[test]
    fn assess_fresh_when_anchor_equals_current() {
        let head = hex40('a');
        let meta = meta_with(Some(&head), Some(&["src/lib.rs"]));
        let changed = |_: &str| -> Option<HashSet<String>> {
            panic!("changed() must not be called when anchor_head == current_head")
        };
        let verdict = super::assess(&meta, &head, &changed);
        assert!(!verdict.stale);
    }

    /// Extra case: `files` present but empty -> fresh (never stale on a
    /// bare HEAD mismatch with no anchored files).
    #[test]
    fn assess_fresh_when_files_empty() {
        let anchor = hex40('a');
        let current = hex40('b');
        let meta = meta_with(Some(&anchor), Some(&[]));
        let changed = |_: &str| -> Option<HashSet<String>> {
            panic!("changed() must not be called when files is empty")
        };
        let verdict = super::assess(&meta, &current, &changed);
        assert!(!verdict.stale);
    }

    /// Extra case: `files` key absent entirely -> fresh.
    #[test]
    fn assess_fresh_when_files_absent() {
        let anchor = hex40('a');
        let current = hex40('b');
        let meta = meta_with(Some(&anchor), None);
        let changed = |_: &str| -> Option<HashSet<String>> {
            panic!("changed() must not be called when files is absent")
        };
        let verdict = super::assess(&meta, &current, &changed);
        assert!(!verdict.stale);
    }

    /// Extra case: a malformed (non-40-hex) `git_head` value must never be
    /// treated as a real anchor.
    #[test]
    fn assess_fresh_when_git_head_malformed() {
        let meta = meta_with(Some("not-a-real-sha"), Some(&["src/lib.rs"]));
        let changed = |_: &str| -> Option<HashSet<String>> {
            panic!("changed() must not be called for a malformed git_head")
        };
        let verdict = super::assess(&meta, &hex40('b'), &changed);
        assert!(!verdict.stale);
    }

    #[test]
    fn stale_decay_is_frozen_at_point_seven() {
        assert!((super::STALE_DECAY - 0.7).abs() < f32::EPSILON);
    }
}
