//! Plan 05-02 STORE-07 — AS_OF query semantics identical across both
//! backends.
//!
//! Body lands in Task 3; this file ships a B-7 stub so Task 1 compiles.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

use crate::fixtures::FixtureCorpus;

/// B-7 stub — Task 3 replaces with the dual-backend ingest + recall +
/// divergence-comparison body per CONTEXT.md D-12.
pub async fn run(
    moon: &Arc<dyn StoragePort>,
    postgres: &Arc<dyn StoragePort>,
    fixtures: &FixtureCorpus,
) -> anyhow::Result<()> {
    let _ = (moon, postgres, fixtures);
    Ok(())
}
