//! ADD task `mq-typed-client` — live-Moon behavior suite for the typed
//! `moondb::MqClient` queue path (contract FROZEN @ v1, 2026-06-11).
//!
//! Gated behind `moon-it` + `MOON_URL` (default `moon://localhost:6380`),
//! skipping gracefully like the sibling suites so `cargo test --workspace`
//! never needs a live Moon.
//!
//! Red discriminators before Build:
//! - `mq_wire_format_is_body` — today publish writes `partition`+`payload`.
//! - `legacy_entry_missing_body_yields_empty_payload` — today subscribe reads
//!   the `payload` field, so a legacy entry round-trips its bytes.
//!
//! `mq_publish_txn_probe` is an EVIDENCE test: both server outcomes pass; the
//! printed verdict feeds TASK.md §7 OBSERVE.

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_storage_moon::MoonStorage;
use std::time::Duration;
use ulid::Ulid;

mod common;
fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            common::note_moon_unreachable(e);
            None
        }
    }
}

fn scope() -> Scope {
    Scope::new("mq-typed-it").expect("valid scope")
}

/// Per-run unique topic so reruns never see each other's entries.
fn fresh_topic(prefix: &str) -> String {
    format!("{prefix}-{}", Ulid::new().to_string().to_lowercase())
}

/// Scoped stream key exactly as `keyspace::mq_topic` derives it.
fn scoped(topic: &str) -> String {
    format!("lunaris:mq-typed-it:{topic}")
}

/// §2 "publish/subscribe round-trip…" AND-clause: the stream entry must carry
/// the SDK's `body` field, not the legacy `partition`/`payload` pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mq_wire_format_is_body() {
    let Some(moon) = connect_or_skip().await else { return };
    let topic = fresh_topic("wire");
    let payload = Bytes::from_static(b"\x00binary\xff");

    moon.publish(&scope(), &topic, 0, payload.clone()).await.expect("publish");

    // Inspect the raw entry without the storage adapter in the way.
    let mut typed = moon.client().typed();
    let raw: redis::Value = redis::cmd("MQ")
        .arg("POP")
        .arg(scoped(&topic))
        .arg("COUNT")
        .arg(1)
        .query_async(typed.inner_mut())
        .await
        .expect("raw MQ POP");

    let redis::Value::Array(entries) = raw else {
        panic!("expected one entry, got {raw:?}");
    };
    let redis::Value::Array(entry) = entries.into_iter().next().expect("one entry") else {
        panic!("entry shape");
    };
    let redis::Value::Array(fields) = entry.into_iter().nth(1).expect("fields") else {
        panic!("fields shape");
    };
    let field_names: Vec<String> = fields
        .chunks(2)
        .filter_map(|p| match p.first() {
            Some(redis::Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Some(redis::Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        field_names.iter().any(|f| f == "body"),
        "stream entry must carry the `body` field (contract v1); got fields {field_names:?}"
    );
}

/// §2 round-trip: payload bytes verbatim, offset > 0, through the public port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_subscribe_roundtrip_preserves_bytes() {
    let Some(moon) = connect_or_skip().await else { return };
    let topic = fresh_topic("rt");
    let payload = Bytes::from_static(b"\x00binary\xff\xfe");

    let offset = moon.publish(&scope(), &topic, 0, payload.clone()).await.expect("publish");
    assert!(offset > 0, "broker offset must be monotonic-positive, got {offset}");

    let mut stream = moon.subscribe(&scope(), "g", &topic, 0).await.expect("subscribe");
    let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("message within 5s")
        .expect("stream alive")
        .expect("no broker error");
    assert_eq!(msg.payload, payload, "payload bytes must round-trip verbatim");
    assert_eq!(msg.partition, 0, "QueueMsg.partition echoes the subscriber argument");
}

/// §2 ack-before-yield: a consumed entry is never redelivered; DLQ stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ack_prevents_redelivery() {
    let Some(moon) = connect_or_skip().await else { return };
    let topic = fresh_topic("ack");

    moon.publish(&scope(), &topic, 0, Bytes::from_static(b"once")).await.expect("publish");
    let mut stream = moon.subscribe(&scope(), "g", &topic, 0).await.expect("subscribe");
    let _consumed = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("message within 5s")
        .expect("stream alive")
        .expect("no broker error");
    drop(stream);

    // The entry was ACKed before yield: a fresh raw POP must find nothing.
    let mut typed = moon.client().typed();
    let raw: redis::Value = redis::cmd("MQ")
        .arg("POP")
        .arg(scoped(&topic))
        .arg("COUNT")
        .arg(1)
        .query_async(typed.inner_mut())
        .await
        .expect("raw MQ POP");
    let redelivered = matches!(&raw, redis::Value::Array(a) if !a.is_empty());
    assert!(!redelivered, "ACKed entry must not be redelivered, got {raw:?}");

    let depth = moon.queue_depth(&scope(), &topic, 0).await.expect("queue_depth");
    assert_eq!(depth, 0, "DLQ must stay empty after a clean consume");
}

/// §2 queue_length via typed dlq_len: fresh topic reports 0 without error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_depth_zero_on_fresh_topic() {
    let Some(moon) = connect_or_skip().await else { return };
    let topic = fresh_topic("depth");
    // Create the topic through the public path so DLQLEN has a stream to read.
    moon.publish(&scope(), &topic, 0, Bytes::from_static(b"seed")).await.expect("publish");
    let depth = moon.queue_depth(&scope(), &topic, 0).await.expect("queue_depth");
    assert_eq!(depth, 0, "fresh topic has empty dead-letter queue");
}

/// §2 legacy entry: an old-format `partition`+`payload` entry yields an EMPTY
/// payload (lenient drain) instead of wedging the stream or recovering bytes
/// from the legacy field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_entry_missing_body_yields_empty_payload() {
    let Some(moon) = connect_or_skip().await else { return };
    let topic = fresh_topic("legacy");
    let key = scoped(&topic);

    // Plant a legacy-format entry exactly as pre-v1 publish wrote it.
    let mut typed = moon.client().typed();
    redis::cmd("MQ")
        .arg("CREATE")
        .arg(&key)
        .query_async::<()>(typed.inner_mut())
        .await
        .expect("raw MQ CREATE");
    let _id: String = redis::cmd("MQ")
        .arg("PUSH")
        .arg(&key)
        .arg("partition")
        .arg(0u16)
        .arg("payload")
        .arg(b"legacy-bytes".as_slice())
        .query_async(typed.inner_mut())
        .await
        .expect("raw legacy MQ PUSH");

    let mut stream = moon.subscribe(&scope(), "g", &topic, 0).await.expect("subscribe");
    let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("legacy entry must drain within 5s (never wedge)")
        .expect("stream alive")
        .expect("lenient path is not an error");
    assert!(
        msg.payload.is_empty(),
        "entry without `body` must yield EMPTY payload (contract v1), got {:?}",
        msg.payload
    );
}

/// §1 publish_txn EVALUATION (evidence test — BOTH outcomes pass).
///
/// Records whether Moon v0.3.0 handles `MQ PUBLISH` inside TXN with
/// visible-only-after-commit semantics. The printed `PUBLISH_TXN VERDICT:`
/// line is copied verbatim into TASK.md §7 OBSERVE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mq_publish_txn_probe() {
    let Some(moon) = connect_or_skip().await else { return };
    let topic = fresh_topic("txn");
    let key = scoped(&topic);

    // Pre-create the topic outside the TXN so visibility is the only variable.
    let mut typed = moon.client().typed();
    let mut mq = typed.mq();
    mq.create(&key, None).await.expect("MQ CREATE outside txn");

    typed.txn_begin().await.expect("TXN.BEGIN");
    let publish_result = typed.mq().publish_txn(&key, b"txn-msg").await;

    match publish_result {
        Err(e) => {
            typed.txn_abort().await.ok();
            eprintln!("PUBLISH_TXN VERDICT: server-rejects MQ PUBLISH — {e}");
        }
        Ok(()) => {
            // Visibility check from an INDEPENDENT connection while txn is open.
            let other =
                MoonStorage::connect(&url()).await.expect("second connection for visibility");
            let other_typed = other.client().typed();
            let before = other_typed.mq().pop(&key, 1).await.expect("pop before commit");
            let visible_before = !before.is_empty();

            typed.txn_commit().await.expect("TXN.COMMIT");

            let after = other_typed.mq().pop(&key, 1).await.expect("pop after commit");
            let visible_after = !after.is_empty();

            eprintln!(
                "PUBLISH_TXN VERDICT: accepted; visible_before_commit={visible_before} \
                 visible_after_commit={visible_after} (atomic iff false/true)"
            );
            assert!(
                !visible_before || visible_after,
                "a message visible before commit must not vanish after commit"
            );
        }
    }
}
