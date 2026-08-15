//! STORE-07 — Moon's `AS_OF` gap, declared and enforced.
//!
//! Through 0.6.x this file was `run_as_of_parity.rs`: it ingested one fixture
//! corpus into Moon AND Postgres and compared historical reads. 0.7.0 deleted
//! the Postgres backend, so there is no second arm to compare against and the
//! parity half is gone with it.
//!
//! What remains is the half that was always the load-bearing one. The live
//! parity run skipped whenever a Moon or a Postgres was missing — which in CI
//! was always — so for a long stretch nothing in a normal `cargo test` run had
//! an opinion about Moon's `AS_OF` behaviour, and `read_as_of` answered
//! historical pins with present-time data unchallenged. [`moon_declares_its_as_of_gap`]
//! runs UNCONDITIONALLY (no service, no env) and pins the declaration AND the
//! refusal, so a change that flips `HISTORICAL_KV_READS` to `true`, deletes the
//! guard, or widens the "latest" window to swallow real time-travel queries
//! fails here.
//!
//! This is also the capability-honest arm the memory-service `as_of` pin was
//! re-expressed against in 0.7.0 (see the port plan §6 item 2): with no
//! bi-temporal backend left in the workspace, "prove time travel works" is not
//! a claim anything can make. "Prove the refusal is explicit and the hot path
//! is untouched" is.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_storage_moon::as_of;

/// Moon must both DECLARE that it cannot serve historical KV reads and REFUSE
/// them explicitly.
///
/// Assertion 1 is the declaration consumers route on
/// (`StoragePort::supports_historical_kv_reads`, wired to this constant).
/// Assertions 2-4 are the behaviour: a genuine time-travel pin is refused
/// with the `NotSupported` variant `lunaris-server` maps to `501
/// not_supported`, while latest-state reads — the entire production hot
/// path — pass through untouched.
// Asserting on a `const` is exactly the point here: the constant IS the
// declaration downstream code routes on, and this test is the tripwire that
// fires the day someone edits it.
#[allow(clippy::assertions_on_constants)]
#[test]
fn moon_declares_its_as_of_gap() {
    assert!(
        !as_of::HISTORICAL_KV_READS,
        "Moon KV rows are plain hashes with no version chain. If this flips to `true`, the \
         upstream TemporalKvIndex (record/get_at) must actually be wired to the KV write path \
         AND kv::read_as_of must read through it — not merely be declared."
    );

    let last_week = Hlc::from_parts(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_sub(7 * 24 * 60 * 60 * 1_000),
        0,
        0,
    );
    let refused = as_of::reject_historical_read(last_week);
    assert!(
        matches!(refused, Err(StorageError::NotSupported(_))),
        "a one-week-old pin is a real time-travel query and MUST be refused with \
         StorageError::NotSupported, got {refused:?}"
    );
    assert!(as_of::reject_historical_read(Hlc::ZERO).is_err(), "an epoch pin must be refused too");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    assert!(
        as_of::reject_historical_read(Hlc::from_parts(now_ms, 0, 0)).is_ok(),
        "latest-state reads (what hydrate/forget/verify/HTTP all issue) must never be refused"
    );
}
