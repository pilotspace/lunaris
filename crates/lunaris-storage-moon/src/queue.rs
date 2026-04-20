//! Plan 03 / Task 1 stub — real impl in Task 3.
//!
//! Task 3 fills `publish` (`MQ.PUSH`) and `subscribe` (`MQ.POP` polling stream).

use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;

use crate::client::MoonClient;

pub(crate) async fn publish(
    _c: &MoonClient,
    _topic: &str,
    _partition: u16,
    _payload: Bytes,
) -> Result<u64, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 3",
    ))
}

pub(crate) async fn subscribe(
    _client: MoonClient,
    _group: &str,
    _topic: &str,
    _partition: u16,
) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 3",
    ))
}
