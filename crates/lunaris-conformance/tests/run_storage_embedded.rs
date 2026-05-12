//! Storage-suite conformance for the embedded SQLite backend (`memory://`).
//!
//! Unlike the Moon / Postgres thin entries this one needs no external service
//! and no env probe — `EmbeddedStorage::connect("memory://")` always succeeds —
//! so the implemented subset runs unconditionally on every `cargo test`.
//!
//! Coverage today (onboarding-overhaul phase 1, skeleton):
//!
//! - [`atomic_write::all_or_nothing`] + [`atomic_write::lsn_monotonic`]
//! - [`scan_range::prefix`]
//! - [`read_as_of::snapshot`]
//!
//! The full [`run_full_storage_suite`](lunaris_conformance::run_full_storage_suite)
//! also exercises `vector_search` (unconditionally) and `graph_traverse` /
//! `queue` (capability-gated). The embedded backend reports
//! `graph_native = queue_native = false` so those skip, but `vector_search`
//! still returns `NotSupported` — hence the full-suite test below is
//! `#[ignore]`d until the brute-force cosine + FTS5 BM25 impls land. Run it
//! explicitly once they do: `cargo test -p lunaris-conformance --test
//! run_storage_embedded -- --ignored`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris_conformance::storage::{atomic_write, read_as_of, scan_range};
use lunaris_core::StoragePort;
use lunaris_storage_embedded::EmbeddedStorage;

async fn mem_storage() -> Arc<dyn StoragePort> {
    Arc::new(EmbeddedStorage::connect("memory://").await.expect("open memory:// backend"))
}

#[tokio::test]
async fn embedded_storage_kv_conformance() -> anyhow::Result<()> {
    let storage = mem_storage().await;
    atomic_write::all_or_nothing(&storage).await?;
    atomic_write::lsn_monotonic(&storage).await?;
    scan_range::prefix(&storage).await?;
    read_as_of::snapshot(&storage).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "embedded backend: vector_search/keyword not implemented yet — un-ignore once they land"]
async fn embedded_storage_full_conformance() -> anyhow::Result<()> {
    let storage = mem_storage().await;
    lunaris_conformance::run_full_storage_suite(storage).await
}
