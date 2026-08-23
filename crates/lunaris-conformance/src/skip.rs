//! Strict mode — in CI, a skip is a failure.
//!
//! Every live-backend runner in this crate short-circuits to `Ok(())` when its
//! backend is unreachable or its binary is unbuilt. That is the right default
//! on a laptop: a developer without a Moon gets a green suite instead of a wall
//! of connection errors. It is the wrong default in CI, where
//! `integration.yml` stands up a Moon, waits for its port, and builds the
//! server before any test runs — there, every precondition is satisfied by
//! construction, so a skip means the job quietly stopped testing and reported
//! success anyway.
//!
//! That distinction is not academic for this crate. The `/v1/forget` P0 — 200
//! OK with nothing deleted, for every real tenant — is guarded by exactly one
//! assertion, `protocol::forget::two_step_hard_delete`, and that assertion runs
//! only inside `tests/run_protocol_lunaris_server.rs`, which has three separate
//! paths that print a SKIP and return `Ok(())`. Any one of them firing in CI
//! leaves the P0 completely unguarded behind a green check — the same shape as
//! the parked test that failed to detect that P0 for four releases.
//!
//! So: `LUNARIS_CONFORMANCE_STRICT=1` turns each skip into a failure, and CI
//! sets it. The flag is opt-in rather than opt-out on purpose — the failure
//! mode of getting it backwards is a contributor's first `cargo test` drowning
//! in errors they cannot act on.

/// Whether skips must fail. Read per call, never cached: tests in one binary
/// share a process, and a cached value would be one more piece of shared state
/// of exactly the kind this module exists to discourage.
pub fn strict() -> bool {
    std::env::var("LUNARIS_CONFORMANCE_STRICT").as_deref() == Ok("1")
}

/// Record a skip — quietly outside strict mode, fatally inside it.
///
/// Returns `Ok(())` so a caller can `return skip_or_fail(reason);` in place of
/// the `eprintln!` + `return Ok(())` pair it replaces, keeping the skip's
/// control flow identical and its meaning conditional.
pub fn skip_or_fail(runner: &str, reason: impl std::fmt::Display) -> anyhow::Result<()> {
    skip_or_fail_with(runner, reason, strict())
}

/// The decision, with the environment passed in rather than read.
///
/// Splitting it is what makes the branch testable at all. The test that used
/// to cover it declared a local `decide(strict: bool)` that re-implemented the
/// `if` and asserted on THAT — so it passed whatever `skip_or_fail` did, and
/// would have stayed green if this function started returning `Ok(())` in both
/// arms, which is precisely the bug it was written to catch. Taking the flag
/// as a parameter also keeps the test off `set_var`, which edition 2024 makes
/// `unsafe` because it races every sibling in the same binary.
pub fn skip_or_fail_with(
    runner: &str,
    reason: impl std::fmt::Display,
    strict: bool,
) -> anyhow::Result<()> {
    if strict {
        anyhow::bail!(
            "{runner}: refusing to skip under LUNARIS_CONFORMANCE_STRICT=1 — {reason}. \
             This job is expected to have every precondition satisfied, so a skip here \
             means the suite stopped testing and would otherwise have reported success. \
             Fix the precondition (is the Moon up? was the binary built?) rather than \
             clearing the flag."
        );
    }
    eprintln!("SKIP {runner}: {reason}");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The helper's whole job is to branch, so both directions are asserted —
    /// against the REAL function. The previous version of this test declared a
    /// local `decide(strict: bool)` with the same `if` inside and asserted on
    /// that instead, which is a fact about the test, not about `skip_or_fail`.
    #[test]
    fn a_skip_is_fatal_only_under_strict_mode() {
        // Process env is never mutated here, for the reason the module docs
        // give; the flag arrives as an argument.
        assert!(
            super::skip_or_fail_with("a_runner", "no Moon", true).is_err(),
            "strict mode must turn a skip into a failure"
        );
        assert!(
            super::skip_or_fail_with("a_runner", "no Moon", false).is_ok(),
            "a skip outside strict mode must stay green"
        );
    }

    /// The parse itself, read-only against whatever the ambient value is.
    ///
    /// This replaces `assert_eq!(Some("1"), Some("1"))` plus
    /// `assert!(!matches!(Some("true"), Some("1")))` — two facts about
    /// literals that never called `strict()` and would have stayed green if
    /// this module started accepting `"true"`.
    ///
    /// What it catches, honestly stated: it pins `strict()` to the documented
    /// parse for the value the process actually has. Under `integration.yml`
    /// that value is `"1"`, so a `strict()` broken to always-false fails here
    /// in the one job where a wrong answer costs something. Locally, with the
    /// variable unset, it pins the `None` arm. It never calls `set_var` —
    /// flipping the variable would race every sibling in this binary,
    /// including siblings that reach it through `skip_or_fail`.
    #[test]
    fn strict_matches_the_documented_parse_for_the_ambient_value() {
        let ambient = std::env::var("LUNARIS_CONFORMANCE_STRICT").ok();
        assert_eq!(
            super::strict(),
            ambient.as_deref() == Some("1"),
            "strict() disagreed with the documented parse for ambient value {ambient:?};              only the exact string \"1\" enables strict mode, so              LUNARIS_CONFORMANCE_STRICT=true must NOT silently enable it"
        );
    }
}
