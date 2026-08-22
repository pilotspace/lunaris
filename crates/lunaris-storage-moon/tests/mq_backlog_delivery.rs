//! F25 root cause — a queue backlog must not silently lose messages.
//!
//! Gated behind `moon-it`. To run:
//!
//! ```bash
//! MOON_TEST_BINARY=/path/to/moon \
//!   cargo test -p lunaris-storage-moon --features moon-it \
//!   --test mq_backlog_delivery -- --nocapture
//! ```
//!
//! ## The defect (moon#652)
//!
//! `MQ POP <key> COUNT <n>` claims `n + max_delivery_count` entries but returns
//! at most `n`. The surplus lands in the group PEL with `last_delivered_id`
//! advanced past it, is never handed to any client, and is never ACKed. Since
//! `read_group_new` reads only `>` and MQ exposes no XAUTOCLAIM, those messages
//! are unreachable forever.
//!
//! `max_delivery_count` defaults to 3, and this adapter's `subscribe` polls
//! `COUNT 1`, so every poll against a backlog deeper than one destroyed up to
//! three messages. Measured on a live Moon: four pushes, one `COUNT 1` pop
//! returned `g1` and left `pending 4` with `last-delivered-id` at the tail —
//! `g2`, `g3` and `g4` were gone. That is why roughly half of all hook-captured
//! chunks were never promoted, for over a month, with nothing in the logs.
//!
//! Lunaris creates its topics with `MAXDELIVERY 0`, which disables DLQ routing
//! and makes the claim exactly `count`. The trade is dead-lettering for
//! at-least-once delivery, which on a promotion queue is plainly the right way
//! round. `MQ CREATE` on an existing topic updates the setting, so a topic
//! damaged by the old default heals on its next publish — the messages already
//! stranded do not come back.
//!
//! ## What this test asserts
//!
//! Behaviour against a live Moon: every message published is delivered.

#![cfg(feature = "moon-it")]

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::{Scope, StoragePort};
use lunaris_storage_moon::MoonStorage;
use lunaris_test_harness::EphemeralMoon;

const TOPIC: &str = "__lunaris_backlog_test__";
const GROUP: &str = "lunaris-backlog-test-v0";

async fn private_moon(test: &str) -> Option<EphemeralMoon> {
    match EphemeralMoon::spawn().await {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("{test}: no ephemeral Moon ({e}); SKIP");
            None
        }
    }
}

/// The whole finding in one assertion: publish a burst, then drain it.
///
/// Five is deliberately more than `max_delivery_count` (3) so the old default
/// cannot pass by accident — under it the first poll returned one message and
/// stranded the other four.
#[tokio::test]
async fn a_backlog_deeper_than_one_is_delivered_in_full() {
    let Some(moon) = private_moon("a_backlog_deeper_than_one_is_delivered_in_full").await else {
        return;
    };
    let storage = MoonStorage::connect_with_dim(moon.url(), 768).await.expect("connect");
    let scope = Scope::new("test.mq.backlog").unwrap();

    let sent: Vec<String> = (1..=5).map(|i| format!("message-{i}")).collect();
    for body in &sent {
        storage.publish(&scope, TOPIC, 0, Bytes::from(body.clone())).await.expect("publish");
    }

    // Subscribe only AFTER the whole burst is queued — that is the shape the
    // promotion worker hits, and the shape that lost messages.
    let mut stream = storage.subscribe(&scope, GROUP, TOPIC, 0).await.expect("subscribe");
    let mut got: Vec<String> = Vec::new();
    for _ in 0..sent.len() {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(Ok(msg))) => got.push(String::from_utf8_lossy(&msg.payload).into_owned()),
            other => panic!(
                "delivery stopped after {} of {} messages ({other:?}); got {got:?}",
                got.len(),
                sent.len()
            ),
        }
    }

    got.sort();
    let mut want = sent.clone();
    want.sort();
    assert_eq!(got, want, "every published message must be delivered");
}

/// Vacuity floor for the test above: prove the harness can observe a message at
/// all, so "all five arrived" cannot be satisfied by a stream that never runs.
#[tokio::test]
async fn a_single_message_round_trips() {
    let Some(moon) = private_moon("a_single_message_round_trips").await else {
        return;
    };
    let storage = MoonStorage::connect_with_dim(moon.url(), 768).await.expect("connect");
    let scope = Scope::new("test.mq.single").unwrap();

    storage.publish(&scope, TOPIC, 0, Bytes::from_static(b"solo")).await.expect("publish");
    let mut stream = storage.subscribe(&scope, GROUP, TOPIC, 0).await.expect("subscribe");
    let msg = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("no message within 10s")
        .expect("stream ended")
        .expect("stream error");
    assert_eq!(&msg.payload[..], b"solo");
}
