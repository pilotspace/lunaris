//! The one skip decision, for every crate's live-fixture tests.
//!
//! ## Why it lives here
//!
//! Four crates grew their own copy of this: `lunaris-storage-moon`'s
//! `tests/common/mod.rs`, `lunaris-conformance`'s `src/skip.rs`,
//! `lunaris-ingest`'s private `note_unreachable`, and a dozen files that
//! skipped with a bare `eprintln!` and no strict mode at all. The copies
//! diverged in wording, and the guards written to police them were keyed on
//! the wording — so each guard saw its own crate's family and went blind to
//! the next one. That is F27, and the fix for the CAUSE is one helper rather
//! than a fifth guard.
//!
//! Placing it in `src/` (not a test-local `common/` module) also makes the
//! workspace sweep in `tests/no_silent_skip_workspace.rs` exactly right: the
//! SKIP print lives outside every `tests/` tree, so "a test file that prints a
//! skip decided it alone" holds with no exceptions to carve out.
//!
//! ## The rule
//!
//! A skip is a courtesy on a developer's box and a defect in a job that
//! guaranteed the fixture. `.github/workflows/integration.yml` builds a Moon,
//! port-checks it, and sets `LUNARIS_CONFORMANCE_STRICT=1` at job level — so a
//! missing fixture there means the fixture broke, and skipping would report
//! success for a suite that tested nothing.
//!
//! The variable keeps its historical name deliberately: ONE switch for the
//! integration job. A second name would let an operator turn half the job
//! strict.

/// Does the caller's environment forbid skipping?
pub fn strict() -> bool {
    std::env::var("LUNARIS_CONFORMANCE_STRICT").as_deref() == Ok("1")
}

/// Record that a live fixture is unavailable — skipping on a dev box,
/// panicking in a job that promised it.
///
/// `what` should name the missing thing and, where it helps, how to supply it:
/// `"no Moon binary (set MOON_TEST_BINARY)"`.
pub fn note_unavailable(what: impl std::fmt::Display) {
    note_unavailable_with(what, strict())
}

/// The decision, with the environment passed in rather than read.
///
/// Splitting it is not ceremony. Reading the variable inside the decision
/// means a test that flips it races every sibling in the same binary —
/// including siblings that never name it, because they reach it through the
/// function under test. Edition 2024 makes `set_var` `unsafe` for exactly this
/// reason. Taking it as a parameter lets the guard exercise both arms without
/// touching the process environment at all.
pub fn note_unavailable_with(what: impl std::fmt::Display, strict: bool) {
    assert!(
        !strict,
        "{what} — and LUNARIS_CONFORMANCE_STRICT=1 forbids skipping. The job that sets it \
         builds the fixture and checks it before running anything, so an unavailable fixture \
         here means the fixture broke, not that the environment lacks one. Skipping would \
         report success for a suite that tested nothing. Unset the variable to run against \
         whatever happens to be up locally."
    );
    eprintln!("SKIP: {what}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The discriminating half. A scanner that only reads source cannot tell a
    /// working strict mode from a decorative one — and a decorative one is
    /// what F27 was: guards asserting that skips were "routed" to a helper
    /// nobody had checked could actually refuse.
    ///
    /// Moved here from `lunaris-ingest`'s private copy, which was the only one
    /// of the four that had this test at all.
    #[test]
    fn strict_mode_refuses_to_skip() {
        assert!(
            std::panic::catch_unwind(|| note_unavailable_with("Moon unreachable", true)).is_err(),
            "note_unavailable_with(.., strict = true) returned instead of panicking"
        );
    }

    /// And a dev box must still be able to run the rest of the suite — a
    /// strict mode that cannot be turned off gets turned off wholesale.
    #[test]
    fn a_dev_box_still_skips() {
        note_unavailable_with("Moon unreachable", false);
    }

    /// The environment is a PARAMETER above and read only here, so no test in
    /// this binary can race a sibling by flipping it. `strict()` still has to
    /// be checked, or the parameterisation would be the only thing tested.
    #[test]
    fn strict_reads_exactly_the_documented_value() {
        // Read-only: whatever the ambient value is, `strict()` must agree with
        // it. Never `set_var` — see the note on `note_unavailable_with`.
        let ambient = std::env::var("LUNARIS_CONFORMANCE_STRICT").ok();
        assert_eq!(strict(), ambient.as_deref() == Some("1"));
    }
}
