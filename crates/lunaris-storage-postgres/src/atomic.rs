//! `atomic_write` — sqlx Transaction, per-op INSERT/UPSERT, COMMIT/ROLLBACK.
//!
//! Task 1 ships the function signature; Task 2 fills in the per-`WriteOp` SQL.

use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{Lsn, WriteOp};

use crate::pool::PgClient;

pub(crate) async fn atomic_write(_c: &PgClient, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
    Err(StorageError::NotSupported("filled in Task 2"))
}
