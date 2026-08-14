//! Plan 05-02 — `storage` suite (D-17).
//!
//! Eight test functions, one per `StoragePort` trait method (Phase 1
//! STORE-01). Run them all via [`run_full_storage_suite`].
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
//! [`StoragePort::supports_historical_kv_reads`](lunaris_core::storage::StoragePort::supports_historical_kv_reads)
//! declaration and asserts the matching behaviour in both directions, so
//! "this backend can't do as-of reads" can never be expressed as a skip
//! (0.6.2 task 9).
//!
//! [`as_of_parity::run`] is dual-backend (it takes BOTH a Moon and a
//! Postgres handle) and is NOT part of the per-backend run — invoke it
//! from the dedicated `tests/run_as_of_parity.rs` thin entry instead.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

pub mod as_of_parity;
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
    atomic_write::all_or_nothing(&storage).await?;
    atomic_write::lsn_monotonic(&storage).await?;
    vector_search::recall(&storage).await?;
    if storage.capabilities().graph_native {
        graph_traverse::cypher_subset(&storage).await?;
    } else {
        eprintln!("conformance: SKIP graph_traverse::cypher_subset (graph_native=false)");
    }
    scan_range::prefix(&storage).await?;
    read_as_of::snapshot(&storage).await?;
    // 0.6.2 task 9 — NOT capability-gated on purpose: a backend that cannot
    // answer as-of reads must still prove it refuses them explicitly.
    read_as_of::historical_pin_is_explicit(&storage).await?;
    if storage.capabilities().queue_native {
        queue::publish_subscribe_round_trip(&storage).await?;
    } else {
        eprintln!("conformance: SKIP queue::publish_subscribe_round_trip (queue_native=false)");
    }
    // as_of_parity is dual-backend; not part of the per-backend run — see
    // crates/lunaris-conformance/tests/run_as_of_parity.rs.
    Ok(())
}
