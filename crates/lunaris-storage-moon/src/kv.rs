//! Plan 03 / Task 1 stub — real impl in Task 2.
//!
//! Task 2 fills `read_as_of` (TEMPORAL.SNAPSHOT_AT then HGET) and `scan_range` (HSCAN-based).

use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::Row;

use crate::client::MoonClient;

pub(crate) async fn read_as_of(
    _c: &MoonClient,
    _key: &[u8],
    _as_of: Hlc,
) -> Result<Option<Row<Bytes>>, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 2",
    ))
}

pub(crate) async fn scan_range<'a>(
    _c: &'a MoonClient,
    _prefix: &[u8],
    _as_of: Option<Hlc>,
) -> Result<BoxStream<'a, Result<(Bytes, Bytes), StorageError>>, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 2",
    ))
}
