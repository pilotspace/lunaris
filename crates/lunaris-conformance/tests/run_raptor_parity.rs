//! Phase 29 Plan 04 — RAPTOR idempotency (STORE-02) + parity (STORE-03) tests.
//!
//! ## Fixture 4 — Idempotent re-ingest (STORE-02)
//! Ingest the same document twice; read back community IDs via scan_range;
//! assert the BTreeSet of IDs is unchanged.
//!
//! ## Fixture 5 — Dual-backend parity (STORE-03)
//! SQLite self-parity (unconditional): two independent EmbeddedStorage instances,
//! same document, field-equal Community trees (id, level, parent, sorted members,
//! non-empty summary, summary_embedding == None).
//! Moon/SQLite cross-backend parity: UAT-gated (MOON_URL env var).
//!
//! ## STORE-03 honesty note (required in SUMMARY)
//! STORE-03 Moon/SQLite cross-backend parity is UAT-GATED — requires MOON_URL.
//! Not claimed green in autonomous CI runs. The SQLite self-parity arm
//! (fixture_5_parity_sqlite_self) passes unconditionally and demonstrates
//! determinism at the code level.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris_conformance::raptor::{run_raptor_idempotency_suite, run_raptor_parity_suite};
use lunaris_core::StoragePort;
use lunaris_storage_embedded::EmbeddedStorage;
use lunaris_storage_moon::MoonStorage;

// ---------------------------------------------------------------------------
// Backend constructors
// ---------------------------------------------------------------------------

async fn sqlite_storage() -> Arc<dyn StoragePort> {
    Arc::new(EmbeddedStorage::connect("memory://").await.expect("EmbeddedStorage must connect"))
}

async fn moon_storage(url: &str) -> Arc<dyn StoragePort> {
    Arc::new(MoonStorage::connect(url).await.expect("MoonStorage must connect"))
}

// ---------------------------------------------------------------------------
// Two-tier Moon probe (verbatim pattern from run_storage_moon.rs)
// ---------------------------------------------------------------------------

fn probe_moon(url: Option<String>) -> Option<String> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let url = url?;
    let host_port = url.strip_prefix("moon://")?.split('/').next()?.to_string();
    let timeout = Duration::from_secs(1);
    let addr = host_port.to_socket_addrs().ok()?.next()?;
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "run_raptor_parity: SKIP Moon (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture 4 — Idempotent re-ingest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_4_idempotency_sqlite() -> anyhow::Result<()> {
    run_raptor_idempotency_suite(sqlite_storage().await).await
}

#[tokio::test]
async fn fixture_4_idempotency_moon() -> anyhow::Result<()> {
    let url = match probe_moon(std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    run_raptor_idempotency_suite(moon_storage(&url).await).await
}

// ---------------------------------------------------------------------------
// Fixture 5 — Dual-backend parity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_5_parity_sqlite_self() -> anyhow::Result<()> {
    run_raptor_parity_suite(sqlite_storage().await, sqlite_storage().await).await
}

#[tokio::test]
async fn fixture_5_parity_moon_sqlite() -> anyhow::Result<()> {
    let url = match probe_moon(std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    run_raptor_parity_suite(moon_storage(&url).await, sqlite_storage().await).await
}
