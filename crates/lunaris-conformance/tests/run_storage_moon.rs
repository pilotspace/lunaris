//! Plan 05-02 STORE-05 thin entry — Moon backend.
//!
//! Run with:
//! ```text
//! MOON_URL=moon://localhost:6390 \
//!   cargo test -p lunaris-conformance --test run_storage_moon
//! ```
//!
//! When `MOON_URL` is unset OR the TCP probe fails, the test SKIPs
//! cleanly (exits 0). Mirrors the verbatim Plan 04-03 `probe_backend`
//! pattern from
//! `crates/lunaris-conformance/tests/crash_recovery.rs:105-154`
//! (Shared Pattern 2 in PATTERNS.md):
//!
//! 1. env value must be `Some` (W-7 — never reads/mutates process env
//!    inside the test, just receives the resolved value).
//! 2. URL parses to a `host:port` authority.
//! 3. `host_port.to_socket_addrs()` resolves (W-3 — handles
//!    `localhost:N` form, not just literal IPs).
//! 4. 1-second TCP connect probe succeeds.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn moon_storage_conformance() -> anyhow::Result<()> {
    let url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let storage: Arc<dyn lunaris_core::StoragePort> =
        Arc::new(lunaris_storage_moon::MoonStorage::connect(&url).await?);
    lunaris_conformance::run_full_storage_suite(storage).await
}

/// Verbatim mirror of `crash_recovery.rs::probe_backend` lines 105-154
/// (Shared Pattern 2). W-3 fix: uses `to_socket_addrs()` (resolves
/// hostnames) NOT `SocketAddr::from_str` (literal IPs only). W-7 fix:
/// takes `Option<String>` (the resolved env value), never the env var
/// name itself.
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
        eprintln!("run_storage_moon: SKIP {name} (unknown URL scheme)");
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            // Intentionally do NOT log the full URL — postgres:// URLs
            // can carry credentials in the userinfo segment
            // (T-05-02-01 mitigation).
            eprintln!("run_storage_moon: SKIP {name} (DNS resolution of {host_port} failed)");
            return None;
        }
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "run_storage_moon: SKIP {name} (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}
