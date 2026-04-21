//! Plan 05-02 D-17 + D-12 — `graph_traverse` conformance (capability-gated).
//!
//! Body lands in Task 2; this file ships a B-7 stub so Task 1 compiles.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

/// B-7 stub — Task 2 replaces with real assertions.
///
/// Caller must gate on `storage.capabilities().graph_native == true`
/// before invoking; the stub body asserts that pre-condition in a
/// `debug_assert!` so accidental calls in non-graph backends surface
/// loudly during the Task 2 fill-in.
pub async fn cypher_subset(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    debug_assert!(
        storage.capabilities().graph_native,
        "caller must gate on graph_native"
    );
    let _ = storage;
    Ok(())
}
