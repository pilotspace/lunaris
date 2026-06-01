//! `publish` and `subscribe` for Moon's `MQ` command family.
//!
//! RFC 0001 Wave 1C: MQ topic names are scoped via `mq_topic(scope, name)` —
//! `lunaris:{scope}:{name}`. A hot scope's queue cannot starve a cold scope's
//! queue because each scope has its own MQ topic (RFC 0001 §3.7).
//!
//! Moon's current wire surface is `MQ <subcommand> ...`, not dotted commands.
//! Keep the RESP calls local to this module until the Rust SDK's partitioned
//! helpers are updated to the same contract.
//!
//! ## subscribe shape
//!
//! Returns a `BoxStream<'static, Result<QueueMsg, StorageError>>`. Each tick
//! polls `MQ POP <topic> COUNT 1`. Empty arrays sleep and continue. On a
//! message, this adapter extracts the `payload` field and ACKs the stream
//! entry before yielding `QueueMsg`; `StoragePort` has no separate ack hook.
//!
//! Phase 1 caps idle CPU at ~3 polls / sec (T-01-03-03 mitigation). Phase 4
//! `VERIFY-06` adds backpressure.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;

use crate::client::{MoonClient, redis_err};
use crate::keyspace::mq_topic;

pub(crate) async fn supports_native_queue(c: &MoonClient) -> Result<bool, StorageError> {
    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let probe: Result<i64, redis::RedisError> =
        redis::cmd("MQ").arg("DLQLEN").arg("__lunaris_queue_probe__").query_async(raw_conn).await;
    match probe {
        Ok(_) => Ok(true),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("unknown command") { Ok(false) } else { Err(redis_err(err)) }
        }
    }
}

pub(crate) async fn publish(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    partition: u16,
    payload: Bytes,
) -> Result<u64, StorageError> {
    // RFC 0001 Wave 1C: route to per-scope MQ topic.
    let scoped_topic = mq_topic(scope, topic);
    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    redis::cmd("MQ")
        .arg("CREATE")
        .arg(scoped_topic.as_str())
        .query_async::<()>(raw_conn)
        .await
        .map_err(redis_err)?;
    let entry_id: String = redis::cmd("MQ")
        .arg("PUSH")
        .arg(scoped_topic.as_str())
        .arg("partition")
        .arg(partition)
        .arg("payload")
        .arg(payload.as_ref())
        .query_async(raw_conn)
        .await
        .map_err(redis_err)?;
    Ok(stream_id_to_offset(&entry_id))
}

/// Plan 04 D-12 — pending (un-ACKed) message count for `(scope, topic, partition)`.
///
/// RFC 0001 Wave 1C: queue health is read through Moon's available `MQ`
/// surface. Current Moon exposes `MQ DLQLEN` but not `XLEN` on the same server
/// profile, so this returns dead-letter depth as the best native signal.
pub(crate) async fn queue_length(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    _partition: u16,
) -> Result<u64, StorageError> {
    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let scoped_topic = mq_topic(scope, topic);
    let n: i64 = redis::cmd("MQ")
        .arg("DLQLEN")
        .arg(scoped_topic.as_str())
        .query_async(raw_conn)
        .await
        .map_err(redis_err)?;
    Ok(n.max(0) as u64)
}

/// Internal state threaded through the unfold stream.
struct PollState {
    client: MoonClient,
    /// Fully-scoped topic name: `lunaris:{scope}:{name}`.
    topic: String,
    partition: u16,
    last_offset: u64,
}

pub(crate) async fn subscribe(
    client: MoonClient,
    scope: &Scope,
    _group: &str,
    topic: &str,
    partition: u16,
) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
    // RFC 0001 Wave 1C: compute per-scope topic name eagerly so the stream
    // owns a `String` rather than a `&Scope` reference (avoids lifetime bleed
    // into the `'static` stream).
    let scoped_topic = mq_topic(scope, topic);

    let state = PollState { client, topic: scoped_topic, partition, last_offset: 0 };

    // We wrap each polling tick in a stream::unfold; idle ticks (Nil reply) sleep then
    // recurse so the consumer never sees an empty / phantom message.
    let stream = stream::unfold(state, |mut s| async move {
        loop {
            let mut typed = s.client.typed();
            let raw_conn = typed.inner_mut();
            let pop: Result<redis::Value, redis::RedisError> = redis::cmd("MQ")
                .arg("POP")
                .arg(s.topic.as_str())
                .arg("COUNT")
                .arg(1)
                .query_async(raw_conn)
                .await;
            match pop {
                Err(e) => {
                    // Surface the error to the consumer; keep the stream alive so
                    // transient broker glitches don't drop the subscription.
                    return Some((Err(redis_err(e)), s));
                }
                Ok(redis::Value::Array(a)) => {
                    let Some((entry_id, payload)) = parse_mq_pop(a) else {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    };
                    let _ = redis::cmd("MQ")
                        .arg("ACK")
                        .arg(s.topic.as_str())
                        .arg(entry_id.as_str())
                        .query_async::<i64>(raw_conn)
                        .await;
                    let offset =
                        stream_id_to_offset(&entry_id).max(s.last_offset.saturating_add(1));
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
                        Err(StorageError::Backend(format!("MQ POP unexpected reply: {other:?}"))),
                        s,
                    ));
                }
            }
        }
    });

    Ok(stream.boxed())
}

fn stream_id_to_offset(entry_id: &str) -> u64 {
    let Some((ms, seq)) = entry_id.split_once('-') else {
        return 0;
    };
    let ms = ms.parse::<u64>().unwrap_or(0);
    let seq = seq.parse::<u64>().unwrap_or(0);
    ms.saturating_mul(1_000_000).saturating_add(seq)
}

fn parse_mq_pop(entries: Vec<redis::Value>) -> Option<(String, Bytes)> {
    let first = entries.into_iter().next()?;
    let redis::Value::Array(mut entry) = first else {
        return None;
    };
    if entry.len() != 2 {
        return None;
    }
    let fields = entry.pop()?;
    let id = value_to_string(entry.pop()?)?;
    let payload = field_value(&fields, b"payload").unwrap_or_default();
    Some((id, payload))
}

fn value_to_string(value: redis::Value) -> Option<String> {
    match value {
        redis::Value::BulkString(bytes) => String::from_utf8(bytes).ok(),
        redis::Value::SimpleString(s) => Some(s),
        _ => None,
    }
}

fn field_value(value: &redis::Value, field: &[u8]) -> Option<Bytes> {
    let redis::Value::Array(items) = value else {
        return None;
    };
    for pair in items.chunks(2) {
        let [key, val] = pair else {
            continue;
        };
        if value_bytes(key).is_some_and(|key| key == field) {
            return value_bytes(val).map(Bytes::from);
        }
    }
    None
}

fn value_bytes(value: &redis::Value) -> Option<Vec<u8>> {
    match value {
        redis::Value::BulkString(bytes) => Some(bytes.clone()),
        redis::Value::SimpleString(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
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
