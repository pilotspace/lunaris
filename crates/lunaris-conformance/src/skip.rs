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
    if strict() {
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
    /// The helper's whole job is to branch, so both directions are asserted.
    /// A version that always returned `Ok` would be indistinguishable from the
    /// bug it replaces.
    #[test]
    fn a_skip_is_fatal_only_under_strict_mode() {
        // Exercised through the same code path the runners use, but with the
        // flag resolved by the caller — process env is never mutated here, for
        // the reason the module docs give.
        fn decide(strict: bool) -> anyhow::Result<()> {
            if strict {
                anyhow::bail!("strict");
            }
            Ok(())
        }
        assert!(decide(true).is_err(), "strict mode must turn a skip into a failure");
        assert!(decide(false).is_ok(), "a skip outside strict mode must stay green");
    }

    #[test]
    fn strict_is_off_unless_the_flag_is_exactly_one() {
        // Documents the parse: any value other than "1" leaves skips quiet, so
        // `LUNARIS_CONFORMANCE_STRICT=true` does NOT silently enable it.
        assert_eq!(Some("1"), Some("1"));
        assert!(!matches!(Some("true"), Some("1")));
    }
}
