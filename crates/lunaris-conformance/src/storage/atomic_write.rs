//! Plan 05-02 D-17 — `atomic_write` conformance assertions.
//!
//! Two functions: [`all_or_nothing`] (committed batch is fully readable)
//! and [`lsn_monotonic`] (sequential writes return strictly increasing
//! `Lsn`). Body lands in Task 2; this file ships B-7 stubs so Task 1
//! compiles.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

/// B-7 stub — Task 2 replaces with real assertions.
pub async fn all_or_nothing(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let _ = storage;
    Ok(())
}

/// B-7 stub — Task 2 replaces with real assertions.
pub async fn lsn_monotonic(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let _ = storage;
    Ok(())
}
