//! SessionStart handover context (session-context-inject task).
//!
//! On a detected session switch, the previous session's scratchpad
//! (`scratchpad/{prev}/` — the task-2 per-session pad convention) is
//! enumerated DIRECTLY from storage and rendered into a bounded, scrubbed
//! summary for Claude Code's stdout `additionalContext` hook contract.
//!
//! Ordering window: SessionStart(B) fires BEFORE session B's first MCP tool
//! call, i.e. before the lazy MCP handover consolidates pad A — so pad A is
//! still fully enumerable here. Enumeration is a plain
//! `StoragePort::scan_range` over the scope's `lunaris:{scope}:episode:`
//! KV prefix with a CLIENT-SIDE source-prefix match — embedding-free,
//! handle-free, and identical on every backend.
//!
//! Flag-resolution note (2026-06-12, ⚠ \[contract\] from the freeze): the
//! contracted keyword_search route is IMPOSSIBLE on live Moon — the FT text
//! parser resolves `@field:` against TEXT fields only, so the `source` TAG
//! declared by PERF-MOON-01 is unsearchable (`ERR unknown field 'source'`,
//! measured against moon HEAD). The contracted fallback (filter-as-query)
//! rides the same parser and silently returns zero hits. `scan_range` is the
//! pre-existing listing surface the flag hoped for — on the LOCKED
//! StoragePort trait, no Moon-side change needed. The full `Lunaris` handle
//! is deliberately NOT constructed either — its chunker tokenizer + reranker
//! probe cost ~2 s, far over the 250 ms context budget (measured live).
//!
//! Everything in this module is best-effort: any storage error, empty pad,
//! or budget overrun yields `None` with one stderr warn — the caller's exit
//! code is decided by the ingest result alone (HOOK-06 spirit; the ingest
//! drop budget is untouched).

use std::collections::BTreeMap;

use futures::StreamExt;
use lunaris_core::keyspace::episode_prefix;
use lunaris_core::{Episode, LunarisError, Scope};
use ulid::Ulid;

/// Maximum pad entries rendered into the summary.
pub const MAX_ENTRIES: usize = 8;
/// Maximum total chars of the rendered summary (context.rs prompt-cap precedent).
pub const MAX_CHARS: usize = 1600;
/// Default context-build budget in milliseconds.
pub const DEFAULT_BUDGET_MS: u64 = 250;
/// Per-value snippet cap so 8 entries always fit inside [`MAX_CHARS`].
const VALUE_SNIPPET_CHARS: usize = 160;
/// Hard cap on scanned episode rows per handover (DoS guard for huge scopes;
/// the budget timeout in main.rs bounds wall time independently).
const SCAN_CAP: usize = 10_000;

/// Parse + clamp the `LUNARIS_HOOK_CONTEXT_BUDGET_MS` value:
/// default 250, clamped to 10..=10000. Non-numeric input falls back to the
/// default (mirrors `LUNARIS_HOOK_DROP_AFTER_MS` semantics in main.rs).
pub fn context_budget_ms(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok()).unwrap_or(DEFAULT_BUDGET_MS).clamp(10, 10_000)
}

/// Char-boundary-safe truncation with a `…` marker when shortened.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render `(key, verbatim-value)` pairs from the previous pad into the
/// bounded summary: names `prev_session_id`, lists at most [`MAX_ENTRIES`]
/// entries, total length <= [`MAX_CHARS`], the final text passed through the
/// default [`crate::scrub::ScrubEngine`] (pad values were written by MCP
/// tools and never crossed the hook scrubber). Empty input -> `None`.
pub fn render_summary(prev_session_id: &str, entries: &[(String, String)]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut s = format!(
        "Previous session '{prev_session_id}' left these scratchpad notes \
         (now being consolidated into long-term memory):\n"
    );
    for (k, v) in entries.iter().take(MAX_ENTRIES) {
        let line = format!("- {k}: {}\n", truncate_chars(v, VALUE_SNIPPET_CHARS));
        if s.len() + line.len() > MAX_CHARS {
            break;
        }
        s.push_str(&line);
    }
    crate::scrub::ScrubEngine::from_default_policy().apply(&mut s);
    // A scrub replacement can grow the string — hard-cap after the pass.
    if s.len() > MAX_CHARS {
        s = truncate_chars(&s, MAX_CHARS);
    }
    Some(s)
}

/// Enumerate `scratchpad/{prev_session_id}/` via a scoped episode KV scan and
/// render the summary from the VERBATIM Episode `content` (never the lossy
/// chunk text). ANY failure (IO error, empty pad) returns `None` after a
/// single stderr warn naming `context_inject`.
///
/// Opens one fresh connection from `storage_url` (switch path only — once
/// per session switch). Repeated writes to the same pad key produce multiple
/// episodes sharing one source; the LATEST (highest episode ULID — ULIDs are
/// time-ordered) wins, matching what the reader would see.
pub async fn build_handover_context(
    storage_url: &str,
    scope: &Scope,
    prev_session_id: &str,
) -> Option<String> {
    match build_inner(storage_url, scope, prev_session_id).await {
        Ok(Some(summary)) => Some(summary),
        Ok(None) => {
            tracing::warn!(prev = prev_session_id, "context_inject skipped: previous pad is empty");
            None
        }
        Err(e) => {
            tracing::warn!(
                prev = prev_session_id,
                err = %e,
                "context_inject skipped: enumeration failed"
            );
            None
        }
    }
}

async fn build_inner(
    storage_url: &str,
    scope: &Scope,
    prev_session_id: &str,
) -> Result<Option<String>, LunarisError> {
    let prefix = format!("scratchpad/{prev_session_id}/");
    let storage = lunaris::open(storage_url).await?;

    let key_prefix = episode_prefix(scope);
    let mut stream =
        storage.scan_range(scope, &key_prefix, None).await.map_err(LunarisError::Storage)?;

    // key -> (latest episode ulid, verbatim value); later writes win.
    let mut latest: BTreeMap<String, (Ulid, String)> = BTreeMap::new();
    let mut scanned = 0usize;
    while let Some(item) = stream.next().await {
        let (_k, v) = item.map_err(LunarisError::Storage)?;
        scanned += 1;
        if let Ok(ep) = serde_json::from_slice::<Episode>(&v)
            && let Some(pad_key) = ep.source.strip_prefix(&prefix)
        {
            let slot = latest.entry(pad_key.to_owned()).or_insert((ep.id, ep.content.clone()));
            if ep.id >= slot.0 {
                *slot = (ep.id, ep.content);
            }
        }
        if scanned >= SCAN_CAP {
            tracing::warn!(
                scanned,
                "context_inject: episode scan cap reached — summary may be partial"
            );
            break;
        }
    }

    let entries: Vec<(String, String)> = latest.into_iter().map(|(k, (_, v))| (k, v)).collect();
    Ok(render_summary(prev_session_id, &entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn budget_clamp_and_default() {
        assert_eq!(context_budget_ms(None), 250, "default budget");
        assert_eq!(context_budget_ms(Some("300")), 300, "in-range passes through");
        assert_eq!(context_budget_ms(Some("5")), 10, "below range clamps up");
        assert_eq!(context_budget_ms(Some("99999")), 10_000, "above range clamps down");
        assert_eq!(context_budget_ms(Some("abc")), 250, "non-numeric falls back");
    }

    #[test]
    fn render_names_session_and_keys() {
        let s = render_summary(
            "sess-a",
            &[entry("plan", "\"ship task 3\""), entry("blocker", "\"none\"")],
        )
        .expect("non-empty pad must render");
        // Assertion messages deliberately do NOT interpolate the rendered
        // summary: CodeQL (rust/cleartext-logging) treats the session-id-
        // tainted string reaching a panic message as a log sink.
        assert!(s.contains("sess-a"), "summary must name the previous session");
        assert!(s.contains("plan"), "summary must list the keys");
        assert!(s.contains("blocker"), "summary must list the keys");
    }

    #[test]
    fn render_empty_is_none() {
        assert_eq!(render_summary("sess-a", &[]), None, "empty pad must render nothing");
    }

    #[test]
    fn render_caps_hold() {
        let big: Vec<(String, String)> =
            (0..20).map(|i| entry(&format!("key-{i:02}"), &"x".repeat(400))).collect();
        let s = render_summary("sess-a", &big).expect("must render");
        assert!(s.len() <= MAX_CHARS, "rendered len {} must be <= {MAX_CHARS}", s.len());
        let listed = (0..20).filter(|i| s.contains(&format!("key-{i:02}"))).count();
        assert!(listed <= MAX_ENTRIES, "at most {MAX_ENTRIES} entries; got {listed}");
        assert!(listed >= 1, "at least one entry must survive the caps");
    }

    #[test]
    fn render_scrubs_secrets() {
        let token = format!("ghp_{}", "A".repeat(36));
        let s = render_summary("sess-a", &[entry("cred", &token)]).expect("must render");
        assert!(!s.contains(&token), "raw GitHub token must never reach the summary");
        assert!(s.contains("<REDACTED:GH_TOKEN>"), "scrubbed replacement must appear instead");
    }
}
