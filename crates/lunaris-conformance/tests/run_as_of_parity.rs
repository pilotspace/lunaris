//! Plan 05-02 STORE-07 thin entry — AS_OF parity (Moon vs Postgres).
//!
//! Run with:
//! ```text
//! MOON_URL=moon://localhost:6380 \
//! PG_URL=postgres://postgres:lunaris@localhost/lunaris \
//!   cargo test -p lunaris-conformance --test run_as_of_parity
//! ```
//!
//! Requires BOTH `MOON_URL` AND `PG_URL` set (and both backends
//! reachable via the 1-second TCP probe). When either is missing or
//! unreachable the test SKIPs cleanly (exits 0).
//!
//! Same probe + skip pattern as the per-backend entries — the helper
//! is duplicated verbatim per the "extract when callers ask for it"
//! convention; the third caller justifies promoting this to a shared
//! `tests/common/probe.rs` module if Plan 05-03 ever adds a fourth.
//!
//! ## The skip is backstopped (0.6.2 task 9)
//!
//! This entry skips whenever a live Moon or Postgres is missing, which in
//! CI is always — so for years nothing in a normal `cargo test` run had an
//! opinion about Moon's AS_OF behaviour, and Moon's `read_as_of` answered
//! historical pins with present-time data unchallenged.
//!
//! [`moon_declares_its_as_of_gap`] below runs UNCONDITIONALLY (no service,
//! no env) and pins the declaration + the refusal. A future change that
//! flips Moon's `HISTORICAL_KV_READS` to `true`, deletes the guard, or
//! widens the "latest" window to swallow real time-travel queries fails
//! here even when the live parity run above skips.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_storage_moon::as_of;

/// Non-skipping companion to the live parity run: Moon must both DECLARE
/// that it cannot serve historical KV reads and REFUSE them explicitly.
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

#[tokio::test]
async fn as_of_parity_moon_vs_postgres() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("PG_URL", std::env::var("PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let moon: Arc<dyn lunaris_core::StoragePort> =
        Arc::new(lunaris_storage_moon::MoonStorage::connect(&moon_url).await?);
    let postgres: Arc<dyn lunaris_core::StoragePort> =
        Arc::new(lunaris_storage_postgres::PostgresStorage::connect(&pg_url).await?);
    let fixtures = lunaris_conformance::fixtures::FixtureCorpus::new();

    lunaris_conformance::storage::as_of_parity::run(&moon, &postgres, &fixtures).await
}

/// Verbatim mirror of `crash_recovery.rs::probe_backend` lines 105-154
/// (Shared Pattern 2). See `tests/run_storage_moon.rs::probe_backend`
/// rustdoc for the full W-3 / W-7 discussion.
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if let Some(rest) = url.strip_prefix("moon://") {
        rest.split('/').next()?.to_string()
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
    } else {
        eprintln!("run_as_of_parity: SKIP {name} (unknown URL scheme)");
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            eprintln!("run_as_of_parity: SKIP {name} (DNS resolution of {host_port} failed)");
            return None;
        }
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "run_as_of_parity: SKIP {name} (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}
