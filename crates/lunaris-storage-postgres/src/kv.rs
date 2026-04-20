//! `read_as_of` — bi-temporal predicate on lunaris_kv.
//! `scan_range`  — prefix `LIKE` on lunaris_kv key column.
//!
//! Task 1 ships the function signatures; Task 2 fills in the SQL.

use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::Row as LRow;

use crate::pool::PgClient;

pub(crate) async fn read_as_of(
    _c: &PgClient,
    _key: &[u8],
    _as_of: Hlc,
) -> Result<Option<LRow<Bytes>>, StorageError> {
    Err(StorageError::NotSupported("filled in Task 2"))
}

pub(crate) async fn scan_range<'a>(
    _c: &'a PgClient,
    _prefix: &[u8],
    _as_of: Option<Hlc>,
) -> Result<BoxStream<'a, Result<(Bytes, Bytes), StorageError>>, StorageError> {
    let empty: Vec<Result<(Bytes, Bytes), StorageError>> = Vec::new();
    let _ = stream::iter(empty);
    Err(StorageError::NotSupported("filled in Task 2"))
}
