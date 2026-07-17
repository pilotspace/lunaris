//! engram-soul-loop task 6 (staleness-pass) — pure staleness assessment +
//! the shared verify-agenda sweep.
//!
//! `.add/tasks/staleness-pass/TASK.md` §3 CONTRACT — RED phase: this module
//! declares only the test suite; `assess` / `StaleVerdict` / `STALE_DECAY` /
//! `sweep_and_upsert` land in the GREEN commit.

use std::collections::HashSet;

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
        let changed = |_: &str| -> Option<HashSet<String>> {
            Some(HashSet::from(["README.md".to_owned()]))
        };
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
