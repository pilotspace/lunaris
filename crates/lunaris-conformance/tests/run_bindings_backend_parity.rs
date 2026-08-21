//! Plan 08-04 — per-driver backend parity (Rust driver).
//!
//! Rust-driver entry for the Phase 8 success-criterion #4 test: within
//! a single process, ingest the FixtureCorpus via `Lunaris::open` +
//! `FixtureCorpus::ingest_into` into Moon, then scan and assert the
//! normalized shape matches the committed golden reference at
//! `crates/lunaris-conformance/fixtures/golden/bindings_fixture.json`.
//!
//! Through 0.6.x this ran against BOTH Moon and Postgres; 0.7.0 deleted the
//! second backend, so the surviving parity axis is across the three language
//! drivers, not across substrates.
//!
//! The Python and TypeScript drivers (`lunaris-py/tests/
//! test_backend_parity.py` and `lunaris-ts/__test__/
//! backend_parity.spec.mts`) replicate this exact flow in their
//! languages. Each driver's assertion is INDEPENDENT — no
//! `assert_eq!(rust_rows, py_rows)` or equivalent is made in any of
//! the three test bodies. Per-driver backend parity is the correct
//! interpretation of success criterion #4 (see `08-04-PLAN.md`
//! revision iteration 2 scope-reset note).
//!
//! Run with:
//! ```text
//! LUNARIS_MOON_URL=moon://localhost:6380 \
//!   cargo test -p lunaris-conformance --features bindings-it \
//!     --test run_bindings_backend_parity -- --nocapture
//! ```
//!
//! When the env var is unset the test SKIPs cleanly (exits 0) so
//! local `cargo test --workspace --all-targets --features bindings-it`
//! runs green without a dev-box backend. Mirrors the Plan 04-03 /
//! 05-02 two-tier skip pattern from `tests/run_storage_moon.rs`
//! (Shared Pattern 2 in PATTERNS.md).

#![cfg(feature = "bindings-it")]
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn rust_driver_backend_parity() -> anyhow::Result<()> {
    // Two-tier probe per Plan 04-03 `crash_recovery.rs::probe_backend`
    // pattern (Shared Pattern 2):
    //   1. env value must be Some (W-7 — never reads/mutates process
    //      env inside the test; uses the resolved value).
    //   2. URL parses to a host:port authority.
    //   3. host_port.to_socket_addrs() resolves (W-3 — handles
    //      `localhost:N` form, not just literal IPs).
    //   4. 1-second TCP connect probe succeeds.
    let moon = probe_backend("LUNARIS_MOON_URL", std::env::var("LUNARIS_MOON_URL").ok());

    lunaris_conformance::bindings::run_rust_driver_backend_parity(moon.as_deref()).await
}

/// Verbatim mirror of `run_storage_moon.rs::probe_backend` lines
/// 46-87 (Shared Pattern 2). W-3 fix: uses `to_socket_addrs()`
/// (resolves hostnames) NOT `SocketAddr::from_str` (literal IPs
/// only). W-7 fix: takes `Option<String>` (the resolved env value),
/// never the env var name itself.
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if let Some(rest) = url.strip_prefix("moon://") {
        rest.split('/').next()?.to_string()
    } else {
        eprintln!("run_bindings_backend_parity: SKIP {name} (unknown URL scheme)");
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
            eprintln!(
                "run_bindings_backend_parity: SKIP {name} (DNS resolution of {host_port} failed)"
            );
            return None;
        }
    };
    if addrs.iter().any(|a| TcpStream::connect_timeout(a, timeout).is_ok()) {
        return Some(url);
    }
    eprintln!(
        "run_bindings_backend_parity: SKIP {name} (TCP probe to {host_port} failed within {}ms across {} address(es))",
        timeout.as_millis(),
        addrs.len()
    );
    None
}
