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
//! `COUNT 1` is only safe because topics are created with `MAXDELIVERY 0` —
//! under the server default of 3 a single `COUNT 1` pop claims four entries
//! and returns one, destroying the other three (F25, pilotspace/moon#652). Do
//! not raise `max_delivery_count` here without first reading that issue.
//! `subscribe` asserts the setting for itself rather than trusting a `publish`
//! to have run first: a consumer can attach to a topic no healthy producer has
//! touched since the fix, and that consumer would otherwise go on stranding
//! its own backlog one pop at a time.
//!
//! ## Recovering a pre-fix backlog (F22 step 2)
//!
//! Messages stranded by the old default are NOT lost. `MQ POP` cannot see
//! them — `read_group_new` reads only `>` and `last_delivered_id` is already
//! past them — but they are still in the `__mq_consumers` PEL with their
//! payloads intact, and `XAUTOCLAIM` walks the PEL directly. A subscriber
//! therefore begins with a bounded reclaim sweep and only then polls for new
//! work. On the live personal store that sweep has 111,729 entries to hand
//! back (census, 2026-08-22).
//!
//! This is the one place in the module that issues a raw RESP command. It has
//! to be: `XAUTOCLAIM` is a stream command with no `MqClient` equivalent, and
//! the whole point is to reach entries the MQ surface cannot. The frozen
//! contract forbids raw `MQ` invocations specifically, which this is not.
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
    // publish. Messages already stranded by it are recovered separately, by
    // the reclaim sweep in `subscribe`.
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

/// W4.6 / D6.3 — non-destructive range read over a topic's stream.
///
/// `XRANGE`, not `MQ POP`. The MQ surface is a consumer: it claims entries,
/// advances `last_delivered_id`, and this module ACKs them before yielding, so
/// reading an audit trail through it would consume the trail. `XRANGE` reads
/// the stream directly and mutates nothing, so an operator can run the same
/// query twice and a background subscriber on the same topic never notices.
///
/// This is the second raw RESP command in this module, for the same reason as
/// the first: the frozen `MqClient` contract forbids raw `MQ` invocations
/// specifically, and this is a stream command with no `MqClient` equivalent —
/// reaching past the MQ surface is the whole point.
///
/// Stream ids are `<ms>-<seq>`, so the wall-clock bounds map onto the id space
/// directly: `from_ms` becomes `<ms>-0` and `to_ms` becomes `<ms>-` plus the
/// maximum sequence, both inclusive. An absent bound becomes `-` / `+`.
pub(crate) async fn queue_range(
    c: &MoonClient,
    scope: &Scope,
    topic: &str,
    _partition: u16,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    limit: usize,
) -> Result<Vec<QueueMsg>, StorageError> {
    let scoped_topic = mq_topic(scope, topic);
    let start = from_ms.map(|ms| format!("{ms}-0")).unwrap_or_else(|| "-".to_string());
    let end = to_ms.map(|ms| format!("{ms}-{}", u64::MAX)).unwrap_or_else(|| "+".to_string());

    let mut typed = c.typed();
    let reply: redis::Value = redis::cmd("XRANGE")
        .arg(&scoped_topic)
        .arg(&start)
        .arg(&end)
        .arg("COUNT")
        .arg(limit)
        .query_async(typed.inner_mut())
        .await
        // `query_async` yields a raw `redis::RedisError`, not a `MoonError` —
        // this is a raw stream command, not an `MqClient` call.
        .map_err(crate::client::redis_err)?;

    let redis::Value::Array(rows) = reply else {
        // A topic that has never been written to is an empty read, not an
        // error — same disposition `queue_length` takes.
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let redis::Value::Array(pair) = row else { continue };
        let mut pair = pair.into_iter();
        let Some(id) = pair.next().and_then(|v| redis_string(&v)) else { continue };
        // Contract v1 wire shape: `body <bytes>`. A legacy entry with no
        // `body` reads as EMPTY rather than wedging the range, exactly as the
        // poll and reclaim paths treat it.
        let payload = match pair.next() {
            Some(redis::Value::Array(fields)) => field_value(fields, b"body"),
            _ => Bytes::new(),
        };
        out.push(QueueMsg {
            topic: topic.to_string(),
            partition: _partition,
            offset: stream_id_to_offset(&id),
            payload,
        });
    }
    Ok(out)
}

/// The consumer group and consumer `MQ` uses internally
/// (`vendor/moon/src/shard/mq_exec.rs`). The reclaim sweep has to name them
/// explicitly because it bypasses the MQ surface to read that group's PEL.
const MQ_GROUP: &str = "__mq_consumers";
/// A consumer name distinct from MQ's own `__mq_default`, so a reclaimed entry
/// is visibly attributed to recovery in `XPENDING` output.
const RECLAIM_CONSUMER: &str = "__lunaris_reclaim";
/// How many PEL entries one sweep step asks for.
const RECLAIM_BATCH: usize = 128;

/// Only reclaim entries idle at least this long. Default 30s.
///
/// A live consumer's window between `MQ POP` and `MQ ACK` is sub-millisecond
/// (see `subscribe` — the ack is the statement before the yield), so 30s is
/// four orders of magnitude of headroom. It exists because `subscribe` is not
/// exclusive: a second process attaching to the same topic must not be able to
/// tear an in-flight message out of the first one's hands, which is exactly
/// what a zero threshold would let it do. Stranded entries are months old and
/// clear the bar trivially.
///
/// `LUNARIS_MQ_RECLAIM_IDLE_MS` overrides it; the integration tests set it to
/// zero because they strand and recover within the same second.
fn reclaim_min_idle_ms() -> u64 {
    std::env::var("LUNARIS_MQ_RECLAIM_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000)
}

/// One entry handed back by the reclaim sweep.
struct Reclaimed {
    id: String,
    data: Bytes,
}

/// Internal state threaded through the unfold stream.
struct PollState {
    client: MoonClient,
    /// Fully-scoped topic name: `lunaris:{scope}:{name}`.
    topic: String,
    partition: u16,
    last_offset: u64,
    /// `Some(cursor)` while the PEL sweep is still walking; `None` once it has
    /// wrapped, after which this subscriber only polls for new work. Topics
    /// created with `MAXDELIVERY 0` cannot strand anything new, so one pass is
    /// the whole job.
    reclaim_cursor: Option<String>,
    /// Entries the last sweep step returned and this stream has not yielded.
    reclaimed: std::collections::VecDeque<Reclaimed>,
}

/// One `XAUTOCLAIM` step: returns the next cursor and the entries claimed.
///
/// A server that does not implement `XAUTOCLAIM` ends the sweep rather than
/// failing the subscription — recovery is a bonus, not a precondition for
/// consuming new messages.
async fn reclaim_step(
    client: &MoonClient,
    topic: &str,
    cursor: &str,
) -> (Option<String>, Vec<Reclaimed>) {
    let mut typed = client.typed();
    let reply: Result<redis::Value, _> = redis::cmd("XAUTOCLAIM")
        .arg(topic)
        .arg(MQ_GROUP)
        .arg(RECLAIM_CONSUMER)
        .arg(reclaim_min_idle_ms())
        .arg(cursor)
        .arg("COUNT")
        .arg(RECLAIM_BATCH)
        .query_async(typed.inner_mut())
        .await;

    let Ok(value) = reply else {
        // No such group yet, no XAUTOCLAIM on this build, or a transport
        // glitch. Either way, stop sweeping and get on with new work.
        return (None, Vec::new());
    };
    parse_reclaim_reply(value)
}

/// Split an `XAUTOCLAIM` reply into `(next cursor, entries)`.
///
/// The reply is `[cursor, [[id, [field, value, ...]], ...]]` with an optional
/// third element (deleted ids) on newer servers. A cursor of `0-0` means the
/// scan wrapped, which is reported as `None`.
fn parse_reclaim_reply(value: redis::Value) -> (Option<String>, Vec<Reclaimed>) {
    let redis::Value::Array(parts) = value else {
        return (None, Vec::new());
    };
    let mut it = parts.into_iter();
    let cursor = match it.next() {
        Some(v) => redis_string(&v).unwrap_or_default(),
        None => return (None, Vec::new()),
    };
    let entries = match it.next() {
        Some(redis::Value::Array(rows)) => rows,
        _ => Vec::new(),
    };

    let mut out = Vec::with_capacity(entries.len());
    for row in entries {
        let redis::Value::Array(pair) = row else { continue };
        let mut pair = pair.into_iter();
        let Some(id) = pair.next().and_then(|v| redis_string(&v)) else { continue };
        // Contract v1 wire shape: `body <bytes>`. An entry without a `body`
        // field drains as EMPTY, exactly as the poll path treats it, so a
        // legacy-format straggler cannot wedge the sweep.
        let data = match pair.next() {
            Some(redis::Value::Array(fields)) => field_value(fields, b"body"),
            _ => Bytes::new(),
        };
        out.push(Reclaimed { id, data });
    }

    let next = if cursor.is_empty() || cursor == "0-0" { None } else { Some(cursor) };
    (next, out)
}

/// Pull one field's value out of a flat `[field, value, ...]` array.
fn field_value(fields: Vec<redis::Value>, want: &[u8]) -> Bytes {
    let mut it = fields.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        if redis_bytes(&k).as_deref() == Some(want) {
            return redis_bytes(&v).map(Bytes::from).unwrap_or_default();
        }
    }
    Bytes::new()
}

fn redis_bytes(v: &redis::Value) -> Option<Vec<u8>> {
    match v {
        redis::Value::BulkString(b) => Some(b.clone()),
        redis::Value::SimpleString(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

fn redis_string(v: &redis::Value) -> Option<String> {
    redis_bytes(v).and_then(|b| String::from_utf8(b).ok())
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

    // Assert MAXDELIVERY 0 for ourselves. `publish` does the same, but a
    // consumer must not depend on a producer having run since the F25 fix:
    // attached to a topic still carrying the server default, every `COUNT 1`
    // poll below would strand up to three more messages. A failure here is not
    // fatal — an MQ-less server still has to be able to report that through the
    // stream rather than through a panic at subscribe time.
    {
        let typed = client.typed();
        let _ = typed.mq().create(&scoped_topic, Some(0)).await;
    }

    let state = PollState {
        client,
        topic: scoped_topic,
        partition,
        last_offset: 0,
        // Start the PEL sweep at the beginning of the stream.
        reclaim_cursor: Some("0-0".to_string()),
        reclaimed: std::collections::VecDeque::new(),
    };

    // We wrap each polling tick in a stream::unfold; idle ticks (empty reply)
    // sleep then recurse so the consumer never sees a phantom message. The
    // SDK's lenient parse folds malformed replies into an empty batch, which
    // lands on the same idle path — the stream never terminates on a glitch.
    let stream = stream::unfold(state, |mut s| async move {
        loop {
            // Phase 1: hand back anything the reclaim sweep recovered. These
            // are messages `MQ POP` provably cannot reach, so they go first —
            // a busy topic must not starve its own backlog.
            if let Some(entry) = s.reclaimed.pop_front() {
                let typed = s.client.typed();
                let mut mq = typed.mq();
                let _ = mq.ack(&s.topic, &entry.id).await;
                let offset = stream_id_to_offset(&entry.id).max(s.last_offset.saturating_add(1));
                s.last_offset = offset;
                let queue_msg = QueueMsg {
                    topic: s.topic.clone(),
                    partition: s.partition,
                    offset,
                    payload: entry.data,
                };
                return Some((Ok(queue_msg), s));
            }
            // Phase 2: advance the sweep until it wraps. Bounded: each step
            // claims at most RECLAIM_BATCH, and the cursor only moves forward.
            if let Some(cursor) = s.reclaim_cursor.take() {
                let (next, entries) = reclaim_step(&s.client, &s.topic, &cursor).await;
                s.reclaim_cursor = next;
                if !entries.is_empty() {
                    s.reclaimed.extend(entries);
                    continue;
                }
                if s.reclaim_cursor.is_some() {
                    // Cursor moved but this page held nothing claimable (every
                    // entry was inside the idle threshold). Keep walking.
                    continue;
                }
            }
            // Phase 3: normal polling for new work.
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
