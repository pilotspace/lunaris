//! Plan 05-02 D-17 — `publish` / `subscribe` round-trip conformance.
//!
//! Body lands in Task 2; this file ships a B-7 stub so Task 1 compiles.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

/// B-7 stub — Task 2 replaces with real assertions.
///
/// Caller must gate on `storage.capabilities().queue_native == true`
/// before invoking.
pub async fn publish_subscribe_round_trip(
    storage: &Arc<dyn StoragePort>,
) -> anyhow::Result<()> {
    debug_assert!(
        storage.capabilities().queue_native,
        "caller must gate on queue_native"
    );
    let _ = storage;
    Ok(())
}
