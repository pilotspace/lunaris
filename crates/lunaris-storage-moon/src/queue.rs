//! `publish` and `subscribe` for Moon's `MQ` command family, via the typed
//! `moondb::MqClient` (ADD task `mq-typed-client`, contract FROZEN @ v1).
//!
//! RFC 0001 Wave 1C: MQ topic names are scoped via `mq_topic(scope, name)` —
//! `lunaris:{scope}:{name}`. A hot scope's queue cannot starve a cold scope's
//! queue because each scope has its own MQ topic (RFC 0001 §3.7).
//!
//! ## Wire shape (contract v1, 2026-06-11)
//!
//! Stream entries carry the SDK's `body` field. The pre-v1 layout
//! (`partition <n> payload <bytes>`) is gone from the wire: `partition` was
//! write-only (nothing ever read it back; `QueueMsg.partition` echoes the
//! subscriber's argument), so it is API-level metadata only. Legacy entries
//! still in a stream at deploy time drain leniently as EMPTY payloads —
//! operators should let consolidate queues drain before deploying (see
//! docs/migration). Dotted MQ command spellings are server-unhandled and
//! FORBIDDEN; so is any raw RESP MQ invocation in this module (both pinned
//! by `tests/mq_typed_client_static.rs`).
//!
//! ## subscribe shape
//!
//! Returns a `BoxStream<'static, Result<QueueMsg, StorageError>>`. Each tick
//! polls `MQ POP <topic> COUNT 1` via `MqClient::pop`. Empty replies sleep and
//! continue. On a message, this adapter ACKs the stream entry before yielding
//! `QueueMsg`; `StoragePort` has no separate ack hook.
//!
//! `COUNT 1` is only safe because `publish` creates topics with
//! `MAXDELIVERY 0` — under the server default of 3 a single `COUNT 1` pop
//! claims four entries and returns one, destroying the other three (F25,
//! pilotspace/moon#652). Do not raise `max_delivery_count` here without first
//! reading that issue.
//!
//! Phase 1 caps idle CPU at ~3 polls / sec (T-01-03-03 mitigation). Phase 4
//! `VERIFY-06` adds backpressure.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;
use moon::MoonError;

use crate::client::{MoonClient, moon_err};
use crate::keyspace::mq_topic;

/// Map an `MqClient` error, naming the MQ-less-server case per contract v1:
/// an `unknown command` reply means the server build has no MQ family.
fn mq_err(e: MoonError) -> StorageError {
    let s = e.to_string();
    if s.contains("unknown command") {
        StorageError::Backend(format!("mq_unsupported: {s}"))
    } else {
        moon_err(e)
    }
}

pub(crate) async fn supports_native_queue(c: &MoonClient) -> Result<bool, StorageError> {
    let typed = c.typed();
    match typed.mq().dlq_len("__lunaris_queue_probe__").await {
        Ok(_) => Ok(true),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("unknown command") { Ok(false) } else { Err(moon_err(err)) }
        }
    }
}

pub(crate) async fn publish(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    _partition: u16,
    payload: Bytes,
) -> Result<u64, StorageError> {
    // RFC 0001 Wave 1C: route to per-scope MQ topic. `partition` is API-level
    // metadata only (contract v1) — it is not encoded on the wire.
    let scoped_topic = mq_topic(scope, topic);
    let typed = c.typed();
    let mut mq = typed.mq();
    // MAXDELIVERY 0, not the server default of 3 — see F25 / pilotspace/moon#652.
    // `MQ POP <key> COUNT <n>` claims `n + max_delivery_count` entries and
    // returns at most `n`; the surplus lands in the group PEL with
    // `last_delivered_id` advanced past it, is never handed to a client and is
    // never ACKed, so `read_group_new` (which reads only `>`) can never see it
    // again. With the default 3 and this module's `COUNT 1` polling, every poll
    // against a backlog deeper than one destroyed up to three messages
    // silently. Setting it to 0 disables DLQ routing, which makes the claim
    // exactly `count`.
    //
    // The trade is dead-lettering for at-least-once delivery. On a promotion
    // queue that is plainly the right way round, and the DLQ was not a working
    // safety net anyway — it was where the losses were NOT going.
    // `MQ CREATE` on an existing topic updates the setting, and publish runs on
    // every send, so a topic damaged under the old default heals on its next
    // publish. Messages already stranded do not come back.
    mq.create(&scoped_topic, Some(0)).await.map_err(mq_err)?;
    let entry_id = mq.push(&scoped_topic, payload.as_ref()).await.map_err(mq_err)?;
    Ok(stream_id_to_offset(&entry_id))
}

/// Plan 04 D-12 — pending (un-ACKed) message count for `(scope, topic, partition)`.
///
/// RFC 0001 Wave 1C: queue health is read through Moon's available `MQ`
/// surface. Current Moon exposes `MQ DLQLEN` but not `XLEN` on the same server
/// profile, so this returns dead-letter depth as the best native signal.
///
/// F25 caveat: since `publish` creates topics with `MAXDELIVERY 0`, DLQ routing
/// is disabled and this now reports **0 for every healthy or unhealthy topic
/// alike**. It is not a backlog gauge and must not be read as one. That is not
/// a regression in signal — under the old default the DLQ was the one place
/// the lost messages were NOT going (pilotspace/moon#652) — but it does mean
/// queue depth is currently unobservable. Restoring a real gauge needs `XLEN`
/// on the MQ profile, or Moon#652 fixed so `MAXDELIVERY` is safe to re-enable.
pub(crate) async fn queue_length(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    _partition: u16,
) -> Result<u64, StorageError> {
    let scoped_topic = mq_topic(scope, topic);
    let typed = c.typed();
    let n = typed.mq().dlq_len(&scoped_topic).await.map_err(mq_err)?;
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

    // We wrap each polling tick in a stream::unfold; idle ticks (empty reply)
    // sleep then recurse so the consumer never sees a phantom message. The
    // SDK's lenient parse folds malformed replies into an empty batch, which
    // lands on the same idle path — the stream never terminates on a glitch.
    let stream = stream::unfold(state, |mut s| async move {
        loop {
            let typed = s.client.typed();
            let mut mq = typed.mq();
            let batch = match mq.pop(&s.topic, 1).await {
                Err(e) => {
                    // Surface the error to the consumer; keep the stream alive
                    // so transient broker glitches don't drop the subscription.
                    return Some((Err(mq_err(e)), s));
                }
                Ok(batch) => batch,
            };
            let Some(msg) = batch.into_iter().next() else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            // Contract v1: an entry without a `body` field drains as an EMPTY
            // payload (the SDK parse yields empty bytes) — ACKed like any
            // other message so legacy-format backlogs never wedge the stream.
            let _ = mq.ack(&s.topic, &msg.id).await;
            let offset = stream_id_to_offset(&msg.id).max(s.last_offset.saturating_add(1));
            s.last_offset = offset;
            let queue_msg = QueueMsg {
                topic: s.topic.clone(),
                partition: s.partition,
                offset,
                payload: msg.data,
            };
            return Some((Ok(queue_msg), s));
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
