//! `publish` / `subscribe` — pgmq.
//!
//! Task 1 ships the function signatures; Task 3 fills in the SQL.

use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;

use crate::pool::PgClient;

pub(crate) async fn publish(
    _c: &PgClient,
    _topic: &str,
    _partition: u16,
    _payload: Bytes,
) -> Result<u64, StorageError> {
    Err(StorageError::NotSupported("filled in Task 3"))
}

pub(crate) async fn subscribe(
    _client: PgClient,
    _group: &str,
    _topic: &str,
    _partition: u16,
) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
    Err(StorageError::NotSupported("filled in Task 3"))
}
