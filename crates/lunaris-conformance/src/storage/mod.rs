//! Plan 05-02 — `storage` suite (D-17).
//!
//! Nine test functions covering the `StoragePort` trait surface (Phase 1
//! STORE-01, plus the 0.6.2 historical-read contract). Run them all via
//! [`run_full_storage_suite`].
//!
//! Tests gate themselves on `storage.capabilities()` per CONTEXT.md D-12 +
//! D-17:
//!
//! - `graph_native = false` → skip [`graph_traverse::cypher_subset`] with
//!   a `conformance: SKIP ...` stderr line.
//! - `queue_native = false` → skip
//!   [`queue::publish_subscribe_round_trip`] similarly.
//! - `bi_temporal_native = false` → [`read_as_of::snapshot`] runs with
//!   relaxed expectations (the backend may emulate temporal reads).
//!
//! [`read_as_of::historical_pin_is_explicit`] is deliberately NOT gated: it
//! branches on the backend's own
//! [`StoragePort::supports_historical_kv_reads`]
//! declaration and asserts the matching behaviour in both directions, so
//! "this backend can't do as-of reads" can never be expressed as a skip
//! (0.6.2 task 9).
//!
//! `as_of_parity::run` was dual-backend — it took BOTH a Moon and a Postgres
//! handle and compared their historical reads. It was deleted in 0.7.0 with
//! the second backend: a parity suite with one arm proves nothing. What it was
//! really guarding lives on unconditionally in
//! `tests/run_as_of_moon_gap.rs`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

pub mod atomic_write;
pub mod graph_traverse;
pub mod queue;
pub mod read_as_of;
pub mod scan_range;
pub mod vector_search;

/// Run every storage-suite test against the given backend.
///
/// Skips capability-dependent tests when the backend's
/// [`StorageCapabilities`](lunaris_core::storage::StorageCapabilities)
/// signals the feature is unsupported (CONTEXT.md D-12).
pub async fn run_full_storage_suite(storage: Arc<dyn StoragePort>) -> anyhow::Result<()> {
    // F11 — one fresh partition per invocation, threaded down rather than read
    // from a process-global. The suite used to write fixed keys under
    // `Scope::dev()`, so it was green exactly once per store and red on every
    // re-run; `tests/storage_suite_is_rerunnable.rs` runs this function twice
    // in one process and would still collide if this were cached anywhere.
    let run = crate::suite_scope::SuiteScope::fresh();
    atomic_write::all_or_nothing(&storage, &run).await?;
    atomic_write::lsn_monotonic(&storage, &run).await?;
    vector_search::recall(&storage, &run).await?;
    if storage.capabilities().graph_native {
        graph_traverse::cypher_subset(&storage, &run).await?;
    } else {
        eprintln!("conformance: SKIP graph_traverse::cypher_subset (graph_native=false)");
    }
    scan_range::prefix(&storage, &run).await?;
    read_as_of::snapshot(&storage, &run).await?;
    // 0.6.2 task 9 — NOT capability-gated on purpose: a backend that cannot
    // answer as-of reads must still prove it refuses them explicitly.
    read_as_of::historical_pin_is_explicit(&storage, &run).await?;
    if storage.capabilities().queue_native {
        queue::publish_subscribe_round_trip(&storage, &run).await?;
    } else {
        eprintln!("conformance: SKIP queue::publish_subscribe_round_trip (queue_native=false)");
    }
    Ok(())
}
