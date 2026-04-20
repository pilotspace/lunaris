//! Plan 03 / Task 1 stub — real impl in Task 2.
//!
//! Once Task 2 lands, this module exposes `atomic_write` as `TXN.BEGIN` + per-op fan-out
//! (`HSET`, `FT.UPSERT`, `GRAPH.QUERY MERGE`) + `TXN.COMMIT` / `TXN.ABORT` on error.

use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{Lsn, WriteOp};

use crate::client::MoonClient;

pub(crate) async fn atomic_write(
    _c: &MoonClient,
    _ops: &[WriteOp],
) -> Result<Lsn, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 2",
    ))
}
