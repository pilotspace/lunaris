//! `publish` (`client.mq().push_partitioned(...)`) and `subscribe`
//! (`client.mq().pop_partitioned(...)` polling stream).
//!
//! RFC 0001 Wave 1C: MQ topic names are scoped via `mq_topic(scope, name)` —
//! `lunaris:{scope}:{name}`. A hot scope's queue cannot starve a cold scope's
//! queue because each scope has its own MQ topic (RFC 0001 §3.7).
//!
//! Phase 1.5 retrofit (STORE-09): typed `moon-client::MqClient` calls replace the
//! previous hand-rolled raw RESP `MQ.PUSH` / `MQ.POP` invocations. Lunaris's
//! partitioned `(topic, partition)` queue model is now exposed as typed
//! `push_partitioned` / `pop_partitioned` helpers in moon-client upstream.
//!
//! ## subscribe shape
//!
//! Returns a `BoxStream<'static, Result<QueueMsg, StorageError>>`. Each tick blocks on
//! `client.mq().pop_partitioned(group, topic, partition, 250)` (250 ms long-poll). On
//! `Value::Nil` reply (poll window expired with no message) the stream sleeps 50 ms and
//! continues polling — silently filtered out (no Nil yields). On reply with a message,
//! yields `Ok(QueueMsg)`. On RESP error, yields `Err(StorageError::Backend(_))` and
//! continues — callers may decide whether to drop the stream.
//!
//! Phase 1 caps idle CPU at ~3 polls / sec (T-01-03-03 mitigation). Phase 4
//! `VERIFY-06` adds backpressure.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;

use crate::client::{MoonClient, moon_err, redis_err};
use crate::keyspace::mq_topic;

pub(crate) async fn publish(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    partition: u16,
    payload: Bytes,
) -> Result<u64, StorageError> {
    let typed = c.typed();
    // RFC 0001 Wave 1C: route to per-scope MQ topic.
    let scoped_topic = mq_topic(scope, topic);
    typed.mq().push_partitioned(&scoped_topic, partition, payload.as_ref()).await.map_err(moon_err)
}

/// Plan 04 D-12 — pending (un-ACKed) message count for `(scope, topic, partition)`.
///
/// ## Path 2 (raw `redis::cmd("MQ.LENGTH")`)
///
/// moon-client v0.1.x does not yet expose a typed `MqClient::length`
/// helper, so we use the SAME raw `redis::cmd` escape hatch documented in
/// `kv.rs::scan_range` (which uses HSCAN for the same reason). When
/// moon-client adds a typed wrapper, swap this for the typed call. This is
/// the SECOND raw RESP cmd invocation in `lunaris-storage-moon/src/` and is
/// covered by the same Phase 1.5 retrofit (STORE-09) constraint comment.
///
/// RFC 0001 Wave 1C: MQ.LENGTH is issued against the per-scope topic.
///
/// Returns 0 when MQ.LENGTH replies a negative number (defensive cast — a
/// well-behaved Moon server returns a non-negative i64 here).
pub(crate) async fn queue_length(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    partition: u16,
) -> Result<u64, StorageError> {
    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let scoped_topic = mq_topic(scope, topic);
    let n: i64 = redis::cmd("MQ.LENGTH")
        .arg(scoped_topic.as_str())
        .arg(partition)
        .query_async(raw_conn)
        .await
        .map_err(redis_err)?;
    Ok(n.max(0) as u64)
}

/// Internal state threaded through the unfold stream.
struct PollState {
    client: MoonClient,
    group: String,
    /// Fully-scoped topic name: `lunaris:{scope}:{name}`.
    topic: String,
    partition: u16,
    last_offset: u64,
}

pub(crate) async fn subscribe(
    client: MoonClient,
    scope: &Scope,
    group: &str,
    topic: &str,
    partition: u16,
) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
    // RFC 0001 Wave 1C: compute per-scope topic name eagerly so the stream
    // owns a `String` rather than a `&Scope` reference (avoids lifetime bleed
    // into the `'static` stream).
    let scoped_topic = mq_topic(scope, topic);

    let state = PollState {
        client,
        group: group.to_string(),
        topic: scoped_topic,
        partition,
        last_offset: 0,
    };

    // We wrap each polling tick in a stream::unfold; idle ticks (Nil reply) sleep then
    // recurse so the consumer never sees an empty / phantom message.
    let stream = stream::unfold(state, |mut s| async move {
        loop {
            let typed = s.client.typed();
            let pop = typed
                .mq()
                .pop_partitioned(s.group.as_str(), s.topic.as_str(), s.partition, 250)
                .await;
            match pop {
                Err(e) => {
                    // Surface the error to the consumer; keep the stream alive so
                    // transient broker glitches don't drop the subscription.
                    return Some((Err(moon_err(e)), s));
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
                        Err(StorageError::Backend(format!("MQ.POP unexpected reply: {other:?}"))),
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
        type BS = Pin<Box<dyn Stream<Item = Result<QueueMsg, StorageError>> + Send + 'static>>;
        assert_send_static::<BS>();
    }

    /// RFC 0001 Wave 1C — mq_topic produces scoped topic names.
    #[test]
    fn scoped_topic_format() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        assert_eq!(mq_topic(&scope, "consolidate"), "lunaris:acme.agent-1:consolidate");
        assert_eq!(mq_topic(&scope, "verify"), "lunaris:acme.agent-1:verify");
    }

    /// RFC 0001 Wave 1C — topics from different scopes must not collide.
    #[test]
    fn scoped_topics_differ_across_scopes() {
        let scope_a = lunaris_core::Scope::new("agent-a").unwrap();
        let scope_b = lunaris_core::Scope::new("agent-b").unwrap();
        assert_ne!(mq_topic(&scope_a, "consolidate"), mq_topic(&scope_b, "consolidate"));
    }
}
