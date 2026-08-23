// Each of the nineteen test binaries that includes this module uses a subset
// of it: the eighteen routed suites call `note_moon_unreachable`, while
// `no_silent_moon_skip.rs` calls only `note_moon_unreachable_with` so it can
// exercise both arms without touching the process environment. Rust warns per
// binary, so without this the guard binary alone reports two dead functions —
// and CI runs clippy with `-D warnings`.
#![allow(dead_code)]

//! Shared no-silent-skip discipline for this crate's live-Moon suite.
//!
//! Why this exists
//! ---------------
//! Fourteen test files in `tests/` each grew their own `connect_or_skip`
//! helper. The five textual variants differ only in which constructor they
//! call — the skip DECISION is byte-identical in all fourteen: print `SKIP`,
//! return `None`, and let the caller `return` out of the test. Thirty-five
//! call sites, every one of them green when Moon is unreachable.
//!
//! Inside this crate that is already contradictory (F10, v0.7.0 review):
//! with Moon down, `episode_roundtrip` and `moon_client_smoke` fail loudly
//! while `a_quant_ef_guardrails` SKIPs green. Same crate, same missing
//! dependency, opposite signals — so the suite's colour tells you nothing
//! about whether Moon was there.
//!
//! In CI it is worse than contradictory. `integration.yml` builds a Moon,
//! port-checks it, and only then runs `cargo test -p lunaris-storage-moon
//! --features moon-it -- --include-ignored`. That job already carries the
//! rule, in its own words: *"A skip in THIS job is a defect, not a
//! courtesy."* It enforces it with `LUNARIS_CONFORMANCE_STRICT=1` — which
//! only `lunaris-conformance` reads. The deepest live coverage the storage
//! layer has sat in the same job, under the same guarantee, ignoring the
//! switch. A Moon that failed to come up would have taken the whole suite to
//! silent green.
//!
//! The env var keeps its historical name deliberately: it is ONE switch for
//! the integration job, and every suite that runs there honours it. Adding a
//! second name would mean an operator could turn half the job strict.

/// Does the caller's environment forbid skipping?
///
/// Set by `.github/workflows/integration.yml`, where every precondition is
/// satisfied by construction.
/// Delegates rather than re-reading. A private copy of this parse is how the
/// readers drifted apart: `lunaris/tests/moon_parity.rs` grew an `Ok("true")`
/// arm the other three never had, which would have made
/// `LUNARIS_CONFORMANCE_STRICT=true` mean strict in one suite and permissive
/// here — the "half the job strict" hazard the module docs above warn about,
/// arrived at through the parse instead of a second name.
pub fn strict() -> bool {
    lunaris_test_harness::strict_skip::strict()
}

/// Record that Moon was unreachable — skipping on a dev box, panicking in a
/// job that promised a Moon.
pub fn note_moon_unreachable(err: impl std::fmt::Display) {
    note_moon_unreachable_with(err, strict())
}

/// The decision, with the environment passed in rather than read.
///
/// Splitting it is not ceremony. `tests/a_maintenance_compact.rs` learned
/// this the expensive way: it flipped an env var inside one test to exercise
/// a threshold and raced its own sibling, which read the same variable
/// indirectly through the function under test — a race `grep` could not have
/// found, because the sibling never named the variable. Edition 2024 makes
/// `set_var` `unsafe` for exactly this reason, and each `tests/*.rs` is one
/// binary, so siblings share the process. Pass the flag; do not mutate the
/// environment to test the environment.
pub fn note_moon_unreachable_with(err: impl std::fmt::Display, strict: bool) {
    lunaris_test_harness::strict_skip::note_unavailable_with(
        format!("MOON_URL not reachable ({err})"),
        strict,
    )
}
