//! `publish` (`MQ.PUSH`) and `subscribe` (`MQ.POP` polling stream).
//!
//! ## subscribe shape
//!
//! Returns a `BoxStream<'static, Result<QueueMsg, StorageError>>`. Each tick blocks on
//! `MQ.POP <group> <topic> <partition> COUNT 1 BLOCK 250` (250 ms long-poll). On Nil
//! reply (poll window expired with no message) the stream sleeps 50 ms and continues
//! polling — this is silently filtered out (no Nil yields). On reply with a message,
//! yields `Ok(QueueMsg)`. On RESP error, yields `Err(StorageError::Backend(_))` and
//! continues — callers may decide whether to drop the stream.
//!
//! Phase 1 caps idle CPU at ~3 polls / sec (T-01-03-03 mitigation). Phase 4
//! `VERIFY-06` adds backpressure.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;

use crate::client::{MoonClient, redis_err};

pub(crate) async fn publish(
    c: &MoonClient,
    topic: &str,
    partition: u16,
    payload: Bytes,
) -> Result<u64, StorageError> {
    let mut conn = c.conn();
    let offset: u64 = redis::cmd("MQ.PUSH")
        .arg(topic)
        .arg(partition)
        .arg(payload.as_ref())
        .query_async(&mut conn)
        .await
        .map_err(redis_err)?;
    Ok(offset)
}

/// Internal state threaded through the unfold stream.
struct PollState {
    client: MoonClient,
    group: String,
    topic: String,
    partition: u16,
    last_offset: u64,
}

pub(crate) async fn subscribe(
    client: MoonClient,
    group: &str,
    topic: &str,
    partition: u16,
) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
    let state = PollState {
        client,
        group: group.to_string(),
        topic: topic.to_string(),
        partition,
        last_offset: 0,
    };

    // We wrap each polling tick in a stream::unfold; idle ticks (Nil reply) sleep then
    // recurse so the consumer never sees an empty / phantom message.
    let stream = stream::unfold(state, |mut s| async move {
        loop {
            let mut conn = s.client.conn();
            let pop: Result<redis::Value, _> = redis::cmd("MQ.POP")
                .arg(s.group.as_str())
                .arg(s.topic.as_str())
                .arg(s.partition)
                .arg("COUNT")
                .arg(1)
                .arg("BLOCK")
                .arg(250)
                .query_async(&mut conn)
                .await;
            match pop {
                Err(e) => {
                    // Surface the error to the consumer; keep the stream alive so
                    // transient broker glitches don't drop the subscription.
                    return Some((Err(redis_err(e)), s));
                }
                Ok(redis::Value::Nil) => {
                    // No message in this poll window. 50ms backoff then continue.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                Ok(redis::Value::Array(a)) => {
                    let mut it = a.into_iter();
                    let offset = match it.next() {
                        Some(redis::Value::Int(n)) => n as u64,
                        _ => s.last_offset.saturating_add(1),
                    };
                    let payload = match it.next() {
                        Some(redis::Value::BulkString(b)) => Bytes::from(b),
                        _ => Bytes::new(),
                    };
                    s.last_offset = offset;
                    let msg = QueueMsg {
                        topic: s.topic.clone(),
                        partition: s.partition,
                        offset,
                        payload,
                    };
                    return Some((Ok(msg), s));
                }
                Ok(other) => {
                    return Some((
                        Err(StorageError::Backend(format!(
                            "MQ.POP unexpected reply: {other:?}"
                        ))),
                        s,
                    ));
                }
            }
        }
    });

    Ok(stream.boxed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::Stream;
    use std::pin::Pin;

    /// Compile-time assertion that subscribe's return type is `'static + Send`.
    #[allow(dead_code)]
    fn _subscribe_returns_static_send_stream() {
        fn assert_send_static<T: Send + 'static + ?Sized>() {}
        type BS = Pin<
            Box<
                dyn Stream<Item = Result<QueueMsg, StorageError>>
                    + Send
                    + 'static,
            >,
        >;
        assert_send_static::<BS>();
    }
}
