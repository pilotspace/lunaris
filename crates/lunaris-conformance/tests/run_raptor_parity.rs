//! Phase 29 Plan 04 — RAPTOR idempotency (STORE-02) + parity (STORE-03) tests.
//!
//! ## Fixture 4 — Idempotent re-ingest (STORE-02)
//! Ingest the same document twice; read back community IDs via scan_range;
//! assert the BTreeSet of IDs is unchanged.
//!
//! ## Fixture 5 — Self-parity (STORE-03)
//! Two INDEPENDENT stores, same document: field-equal Community trees (id,
//! level, parent, sorted members, non-empty summary, summary_embedding ==
//! None). This is a determinism proof — the same input must build the same
//! tree twice, in two processes that share nothing.
//!
//! ## What 0.7.0 changed here
//!
//! This file used to be genuinely dual-backend: a SQLite self-parity arm that
//! ran unconditionally, plus Moon and Moon-vs-SQLite arms gated on a `MOON_URL`
//! env var. The port plan recorded the SQLite arms as KEEP-until-deletion (one
//! ARM of a deliberate comparison, not a `memory://` convenience) and flagged
//! the Moon arms as **never actually run** — CI never set `MOON_URL`.
//!
//! 0.7.0 deletes the SQLite arms with their backend and takes the flagged
//! follow-up: `lunaris-test-harness` powers the Moon arms with a disposable
//! child-process Moon each, so they run on every `cargo test` for the first
//! time. Net effect on coverage is an upgrade, not a loss — the determinism
//! claim is now made about the substrate production runs, and it is no longer
//! contingent on an env var nobody sets.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use lunaris_conformance::raptor::{run_raptor_idempotency_suite, run_raptor_parity_suite};
use lunaris_test_harness::open_test_storage;

// ---------------------------------------------------------------------------
// Fixture 4 — Idempotent re-ingest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_4_idempotency_moon() -> anyhow::Result<()> {
    // Bind the fixture to a local: it owns the Moon child, and dropping it
    // mid-test would take the server with it.
    let storage = open_test_storage().await;
    run_raptor_idempotency_suite(storage.port()).await
}

// ---------------------------------------------------------------------------
// Fixture 5 — Self-parity across two independent Moons
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_5_parity_moon_self() -> anyhow::Result<()> {
    let a = open_test_storage().await;
    let b = open_test_storage().await;
    assert_ne!(a.url(), b.url(), "self-parity needs two genuinely separate stores");
    run_raptor_parity_suite(a.port(), b.port()).await
}
