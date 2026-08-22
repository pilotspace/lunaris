//! Plan 05-02 STORE-05 thin entry — Moon backend.
//!
//! Run with:
//! ```text
//! MOON_URL=moon://localhost:6380 \
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
        None => {
            return lunaris_conformance::skip::skip_or_fail(
                "run_storage_moon",
                "MOON_URL not set / reachable",
            );
        }
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
    } else {
        lunaris_test_harness::strict_skip::note_unavailable(format!(
            "run_storage_moon: SKIP {name} (unknown URL scheme)"
        ));
        return None;
    };

    let timeout = Duration::from_secs(1);
    // Every resolved address, not just the first. `to_socket_addrs()`
    // returns them in resolver order, and on macOS `localhost` yields
    // `::1` ahead of `127.0.0.1` — so probing only `next()` reported a
    // perfectly healthy IPv4-bound Moon as unreachable and skipped the
    // whole suite to green. Which address answers is not the question;
    // whether ANY does, is.
    let addrs: Vec<_> = match host_port.to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(_) => {
            // Intentionally never log the URL itself — a store URL can
            // carry credentials. Host:port and the env var name only.
            lunaris_test_harness::strict_skip::note_unavailable(format!(
                "run_storage_moon: SKIP {name} (DNS resolution of {host_port} failed)"
            ));
            return None;
        }
    };
    if addrs.iter().any(|a| TcpStream::connect_timeout(a, timeout).is_ok()) {
        return Some(url);
    }
    lunaris_test_harness::strict_skip::note_unavailable(format!(
        "run_storage_moon: SKIP {name} (TCP probe to {host_port} failed within {}ms across {} address(es))",
        timeout.as_millis(),
        addrs.len()
    ));
    None
}
