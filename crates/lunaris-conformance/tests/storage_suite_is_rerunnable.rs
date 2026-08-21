//! F11 — the storage conformance suite must not assume it owns the store.
//!
//! `moon_storage_conformance` was green against a pristine Moon and red
//! against one it had already run on: `read_as_of::snapshot` asserts a key is
//! absent "before any write", and the previous run had written it. CI never
//! saw it (fresh Moon per job); it bit every developer who ran the suite
//! twice, and it cost two false diagnoses in one session while verifying F10 —
//! a *real* pass and a *stale-state* failure are hard to tell apart when you
//! are already looking for a bug.
//!
//! The failure mode is cross-process, so the honest test is stronger than the
//! bug: run the whole suite **twice in one process against the same live
//! Moon**. Two consecutive greens mean each invocation partitioned itself, and
//! partitioning per invocation implies partitioning per process for free.
//!
//! This lives in its own test binary on purpose. Cargo runs integration
//! binaries one at a time but runs the `#[test]`s *inside* one binary on
//! parallel threads — sharing a file with `run_storage_moon.rs` would have the
//! two suite invocations racing each other on the same store, which is the
//! same collision wearing a different hat and would make the RED unreadable.
//!
//! Skips exactly like its sibling when `MOON_URL` is unset, and hard-fails
//! under `LUNARIS_CONFORMANCE_STRICT=1` — the switch `integration.yml` sets.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn the_storage_suite_survives_a_second_run_on_the_same_store() -> anyhow::Result<()> {
    let url = match probe_backend(std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => {
            return lunaris_conformance::skip::skip_or_fail(
                "storage_suite_is_rerunnable",
                "MOON_URL not set / reachable",
            );
        }
    };

    let storage: Arc<dyn lunaris_core::StoragePort> =
        Arc::new(lunaris_storage_moon::MoonStorage::connect(&url).await?);

    lunaris_conformance::run_full_storage_suite(Arc::clone(&storage))
        .await
        .map_err(|e| anyhow::anyhow!("first run failed — this is not the F11 bug: {e}"))?;

    // The whole point. A suite that hard-codes its keys and its scope reads
    // its own first-run residue here and reports a contract violation that
    // never happened.
    lunaris_conformance::run_full_storage_suite(storage).await.map_err(|e| {
        anyhow::anyhow!(
            "the storage suite is not re-runnable against a store it has already \
             touched: {e}. Each invocation must partition itself — a fresh scope \
             AND fresh keys, since Moon writes KvPut keys verbatim and only the \
             vector/graph legs get a per-scope namespace."
        )
    })
}

/// Same probe as `run_storage_moon.rs`, minus the env-var-name parameter this
/// file has no second caller for. Probes every resolved address: on macOS
/// `localhost` yields `::1` first, and an IPv4-bound Moon read as unreachable
/// would skip this to green.
fn probe_backend(url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = url.strip_prefix("moon://")?.split('/').next()?.to_string();
    let timeout = Duration::from_secs(1);
    let addrs: Vec<_> = host_port.to_socket_addrs().ok()?.collect();
    if addrs.iter().any(|a| TcpStream::connect_timeout(a, timeout).is_ok()) {
        return Some(url);
    }
    // Never log the URL itself — a store URL can carry credentials.
    eprintln!("storage_suite_is_rerunnable: TCP probe to {host_port} failed");
    None
}
