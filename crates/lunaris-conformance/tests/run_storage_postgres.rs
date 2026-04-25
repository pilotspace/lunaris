//! Plan 05-02 STORE-05 thin entry — Postgres backend.
//!
//! Run with:
//! ```text
//! PG_URL=postgres://postgres:lunaris@localhost/lunaris \
//!   cargo test -p lunaris-conformance --test run_storage_postgres
//! ```
//!
//! When `PG_URL` is unset OR the TCP probe fails, the test SKIPs
//! cleanly (exits 0). Same probe + skip pattern as
//! `tests/run_storage_moon.rs` — the helper is duplicated verbatim
//! rather than extracted into a shared module per the "extract when
//! callers ask for it" convention from Plan 02-04 (3rd caller is the
//! threshold; 2 callers tolerate dup).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn postgres_storage_conformance() -> anyhow::Result<()> {
    let url = match probe_backend("PG_URL", std::env::var("PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let storage: Arc<dyn lunaris_core::StoragePort> =
        Arc::new(lunaris_storage_postgres::PostgresStorage::connect(&url).await?);
    lunaris_conformance::run_full_storage_suite(storage).await
}

/// Mirror of `crash_recovery.rs::probe_backend` lines 105-154
/// (Shared Pattern 2). See `tests/run_storage_moon.rs::probe_backend`
/// rustdoc for the full W-3 / W-7 fix discussion.
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
        eprintln!("run_storage_postgres: SKIP {name} (unknown URL scheme)");
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            eprintln!("run_storage_postgres: SKIP {name} (DNS resolution of {host_port} failed)");
            return None;
        }
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "run_storage_postgres: SKIP {name} (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}
