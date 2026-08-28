//! The canonical classification of an episode's `source` string.
//!
//! An episode's source decides how the rest of the system may use it, and
//! before this module two layers answered that question independently:
//! `lunaris-hook` owned the injection answer (W4.4 — raw tool-call captures
//! are substrate, never auto-injected) while `lunaris-consolidate` had no
//! answer at all and fed every captured envelope to the dream planner.
//!
//! Keeping the list in one place is the point. A second copy would drift the
//! moment a new capture kind is added, and the failure it produces is silent:
//! the new kind simply keeps flowing into whichever consumer still has the
//! stale list.

/// True if `source` is a raw tool-call capture rather than a durable record.
///
/// These envelopes are transient execution logs. They are still captured,
/// still stored, and still returned by `memory.recall` — they are substrate.
/// What they are NOT is content a consumer should treat as knowledge:
///
/// - **injection** (`lunaris-hook`): never placed in an agent's context
///   automatically. A census over 1,204 real injection blocks found 99.9% of
///   everything injected was a raw tool call, against two curated entries in
///   the entire history.
/// - **distillation** (`lunaris-consolidate`): never a dream-agenda
///   candidate. The curation-gap ruling (2026-08-20, decision 1) is blunt
///   about why — "you cannot summarize `ls -la` into wisdom". Handing the
///   planner 18k `PostToolUse` envelopes does not produce knowledge, it
///   produces 18k clusters a human has to reject.
///
/// Matched as full literals, not by prefix: `lunaris:` also prefixes
/// `memory_injection`, `turn_feedback` and `session_start`, which are hook
/// bookkeeping with their own (different) handling in
/// `excluded_context_source`. A prefix match here would quietly swallow those
/// too.
pub fn is_toolcall_capture(source: &str) -> bool {
    matches!(
        source,
        "lunaris:tool_call:pre"
            | "lunaris:tool_call:post"
            | "lunaris:pre_tool_use"
            | "lunaris:post_tool_use"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capture_kind_is_recognized() {
        for s in [
            "lunaris:tool_call:pre",
            "lunaris:tool_call:post",
            "lunaris:pre_tool_use",
            "lunaris:post_tool_use",
        ] {
            assert!(is_toolcall_capture(s), "{s} must be recognized as a tool-call capture");
        }
    }

    /// The bookkeeping sources share the `lunaris:` prefix and must NOT be
    /// caught here — they are handled separately by the hook. This is the
    /// assertion that fails if someone "simplifies" the match into a
    /// `starts_with("lunaris:")`.
    #[test]
    fn hook_bookkeeping_and_curated_sources_are_not_captures() {
        for s in [
            "lunaris:memory_injection",
            "lunaris:turn_feedback",
            "lunaris:session_start",
            "lunaris:stop",
            "decision:api-shape",
            "distilled:fact:proj",
            "edit:src/lib.rs",
            "fix:flaky-test",
        ] {
            assert!(!is_toolcall_capture(s), "{s} must NOT be treated as a tool-call capture");
        }
    }
}
