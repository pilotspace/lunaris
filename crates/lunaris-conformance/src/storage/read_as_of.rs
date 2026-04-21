//! Plan 05-02 D-17 — `read_as_of` conformance.
//!
//! Body lands in Task 2; this file ships a B-7 stub so Task 1 compiles.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

/// B-7 stub — Task 2 replaces with real assertions.
pub async fn snapshot(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let _ = storage;
    Ok(())
}
