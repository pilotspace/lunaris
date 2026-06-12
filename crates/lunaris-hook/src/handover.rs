//! SessionStart handover context (session-context-inject task).
//!
//! On a detected session switch, the previous session's scratchpad
//! (`scratchpad/{prev}/` — the task-2 per-session pad convention) is
//! enumerated DIRECTLY from storage and rendered into a bounded, scrubbed
//! summary for Claude Code's stdout `additionalContext` hook contract.
//!
//! Ordering window: SessionStart(B) fires BEFORE session B's first MCP tool
//! call, i.e. before the lazy MCP handover consolidates pad A — so pad A is
//! still fully enumerable here. Enumeration is embedding-free
//! (`StoragePort::keyword_search` + episode-content hydration); the hook
//! NEVER constructs a native embedder.
//!
//! Everything in this module is best-effort: any storage error, backend
//! `NotSupported`, empty pad, or budget overrun yields `None` with one
//! stderr warn — the caller's exit code is decided by the ingest result
//! alone (HOOK-06 spirit; the ingest drop budget is untouched).

use std::sync::Arc;

use lunaris_core::{Scope, StoragePort};

/// Maximum pad entries rendered into the summary.
pub const MAX_ENTRIES: usize = 8;
/// Maximum total chars of the rendered summary (context.rs prompt-cap precedent).
pub const MAX_CHARS: usize = 1600;
/// Default context-build budget in milliseconds.
pub const DEFAULT_BUDGET_MS: u64 = 250;

/// Parse + clamp the `LUNARIS_HOOK_CONTEXT_BUDGET_MS` value:
/// default 250, clamped to 10..=10000. Non-numeric input falls back to the
/// default (mirrors `LUNARIS_HOOK_DROP_AFTER_MS` semantics in main.rs).
pub fn context_budget_ms(raw: Option<&str>) -> u64 {
    let _ = raw;
    DEFAULT_BUDGET_MS // RED stub
}

/// Render `(key, verbatim-value)` pairs from the previous pad into the
/// bounded summary: names `prev_session_id`, lists at most [`MAX_ENTRIES`]
/// entries, total length <= [`MAX_CHARS`], every value passed through the
/// default [`crate::scrub::ScrubEngine`]. Empty input -> `None`.
pub fn render_summary(prev_session_id: &str, entries: &[(String, String)]) -> Option<String> {
    let _ = (prev_session_id, entries);
    None // RED stub
}

/// Enumerate `scratchpad/{prev_session_id}/` via the storage keyword surface,
/// hydrate verbatim values from parent Episode content, and render the
/// summary. ANY failure (keyword `NotSupported` on embedded/sqlite, IO error,
/// empty pad) returns `None` after a single stderr warn naming the reason.
pub async fn build_handover_context(
    storage: Arc<dyn StoragePort>,
    scope: &Scope,
    prev_session_id: &str,
) -> Option<String> {
    let _ = (storage, scope, prev_session_id);
    None // RED stub
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
        assert!(s.contains("sess-a"), "summary must name the previous session: {s}");
        assert!(s.contains("plan"), "summary must list the keys: {s}");
        assert!(s.contains("blocker"), "summary must list the keys: {s}");
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
        assert!(!s.contains(&token), "raw GitHub token must never reach the summary: {s}");
        assert!(s.contains("<REDACTED:GH_TOKEN>"), "scrubbed replacement must appear instead: {s}");
    }
}
