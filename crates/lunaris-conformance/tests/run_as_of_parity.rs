//! Plan 05-02 STORE-07 thin entry — AS_OF parity (Moon vs Postgres).
//!
//! Run with:
//! ```text
//! MOON_URL=moon://localhost:6390 \
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

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

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
