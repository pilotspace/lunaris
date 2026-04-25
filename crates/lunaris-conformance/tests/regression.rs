//! Plan 14-03 — EVAL-05 regression tests module hub.
//!
//! One file per release-blocking bug EVAL-05 surfaced in v0.1.1
//! (see `milestones/v0.1.1-MILESTONE-AUDIT.md` + `.planning/ROADMAP.md`
//! Phase 14 block). Each test is `#[cfg(feature = "pg-it")]`-gated so
//! default `cargo test --workspace` does not attempt a live PG connection.
//!
//! ## EXPECTED_VANILLA_ERRORS — the CI-03 negative-matrix oracle
//!
//! The source of truth for what each regression test emits as its
//! first-line failure substring when run against **vanilla `postgres:16`**
//! (no pgvector, no AGE, no pgmq). The GitHub Actions
//! `integration-vanilla-pg-negative` job greps each per-test log for the
//! substring below; if ANY grep misses, the job fails (means the
//! assertion logic itself is silently green — R14-02 mitigation).
//!
//! ### Honest framing (important)
//!
//! All three PG-dependent tests fail on vanilla pg:16 at the **same
//! point**: `PostgresStorage::connect()` runs the `sqlx::migrate!` set,
//! and the very first migration `20260420000001_init_extensions.sql`
//! begins with `CREATE EXTENSION IF NOT EXISTS vector`. Vanilla pg:16
//! does not ship pgvector, so every test short-circuits with the
//! `extension "vector" is not available` message BEFORE ever reaching
//! the bug-specific code path.
//!
//! This is PRECISELY the CI-substrate-gap thesis CI-03 encodes: CI that
//! boots vanilla pg:16 NEVER exercises the bug paths, which is why the 4
//! v0.1.1 bugs slipped through. The substring is therefore an extension
//! availability signal, not a bug-failure-mode replay.
//!
//! ### The 4th test (candle embed_tokens fallback) is PG-independent
//!
//! Bug 3 (candle `embed_tokens` tensor-path fallback) is not Postgres-
//! dependent — it lives in `lunaris-embed` and exercises a safetensors
//! load path. Its table entry is `None` ("expected to pass on vanilla
//! pg:16 too"). The negative-matrix post-step treats a `None` entry as
//! "assert exit code 0", not "grep for substring".
//!
//! ## Contract with `.github/workflows/integration.yml`
//!
//! The `integration-vanilla-pg-negative` job hardcodes a YAML mirror of
//! this table (denormalized per P14-D06 — 4 entries, trivial to keep
//! in sync; any bump must update BOTH this file AND the YAML). The
//! sanity trip-wire step in the same job runs a 5th synthetic test that
//! is engineered to fail on BOTH backends; if it EVER passes, the
//! assertion logic is broken (R14-02 trip-wire).

#![allow(dead_code)] // `EXPECTED_VANILLA_ERRORS` is read by humans + the
// workflow post-step, not by the Rust code itself.

// Each bug file lives in `tests/regression/<bug>.rs` — one file per the v0.1.1
// release-blocking bug it encodes. Cargo's integration-test discovery only
// picks up `tests/*.rs` (this file) by default, so we `#[path = ...]`-attribute
// the submodules to keep the tests/regression/ directory as the canonical
// layout without introducing a `tests/regression/main.rs` second binary that
// would double-count the suite.
#[path = "regression/bytea_like_byterange_scan.rs"]
mod bytea_like_byterange_scan;
#[path = "regression/candle_embed_tokens_fallback.rs"]
mod candle_embed_tokens_fallback;
#[path = "regression/search_path_session_leak.rs"]
mod search_path_session_leak;
#[path = "regression/sqlx_migration_version_collision.rs"]
mod sqlx_migration_version_collision;

/// Source-of-truth table for the CI-03 negative-matrix grep assertion.
///
/// Pairs a test-function name with its expected-error substring on vanilla
/// `postgres:16`. `None` marks a PG-independent test that is expected to
/// PASS even on vanilla pg:16 (asserted via exit code 0 in the workflow
/// post-step, not by substring grep).
///
/// See module-level rustdoc for why all PG-dependent tests share the same
/// substring — vanilla pg:16 fails at the first `CREATE EXTENSION vector`
/// statement before any test-specific setup runs.
#[cfg(feature = "pg-it")]
pub const EXPECTED_VANILLA_ERRORS: &[(&str, Option<&str>)] = &[
    ("sqlx_migration_version_collision", Some("extension \"vector\" is not available")),
    ("search_path_session_leak", Some("extension \"vector\" is not available")),
    // PG-independent — candle embed_tokens fallback lives in lunaris-embed;
    // vanilla pg:16 passes this test unchanged. The negative-matrix post-step
    // asserts exit code 0 for this entry, not a stderr substring.
    ("candle_embed_tokens_fallback", None),
    ("bytea_like_byterange_scan", Some("extension \"vector\" is not available")),
];
