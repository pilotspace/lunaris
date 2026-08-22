//! F22 step (2) — a topic damaged by the old `MAXDELIVERY` default must be
//! able to recover the messages stranded in its consumer-group PEL.
//!
//! Gated behind `moon-it`. To run:
//!
//! ```bash
//! MOON_TEST_BINARY=/path/to/moon \
//!   cargo test -p lunaris-storage-moon --features moon-it \
//!   --test mq_stranded_recovery -- --nocapture
//! ```
//!
//! ## Why this exists
//!
//! `mq_backlog_delivery.rs` proves new messages survive now that `publish`
//! creates topics with `MAXDELIVERY 0` (F25, pilotspace/moon#652). It says
//! nothing about the messages lost *before* that fix shipped, and there are a
//! lot of them: a read-only census of the live personal store on 2026-08-22
//! found 111,729 entries sitting in the `__mq_consumers` PEL across 465
//! `__lunaris_embed__` topics.
//!
//! Those entries are NOT gone. `MQ POP` cannot reach them — `read_group_new`
//! reads only `>` and `last_delivered_id` is already past them — but
//! `XAUTOCLAIM` walks the PEL directly and returns them with their payloads
//! intact. An earlier note in this repo (and my first version of moon#652)
//! called them "unreachable forever"; that was wrong.
//!
//! ## What these tests assert
//!
//! 1. A subscriber attached to a topic damaged under the old default delivers
//!    the stranded messages, not just the ones still readable as new.
//! 2. A subscriber attached to a healthy topic delivers each message exactly
//!    once — recovery must not turn into duplication.
//! 3. Vacuity floor: the strand this test builds is real, i.e. `MQ POP` really
//!    cannot see those entries. Without this, test 1 could pass on a Moon
//!    where nothing was ever stranded in the first place.

#![cfg(feature = "moon-it")]

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::{Scope, StoragePort};
use lunaris_storage_moon::MoonStorage;
use lunaris_test_harness::EphemeralMoon;

const TOPIC: &str = "__lunaris_strand_test__";
const GROUP: &str = "lunaris-strand-test-v0";

/// Reclaim only entries idle longer than this. Tests need it at zero; see the
/// `RECLAIM_MIN_IDLE_MS` doc comment in `src/queue.rs` for why production does
/// not.
fn force_instant_reclaim() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: run exactly once, before any test in this binary constructs a
    // subscriber, so no other thread is reading the environment concurrently.
    ONCE.call_once(|| unsafe {
        std::env::set_var("LUNARIS_MQ_RECLAIM_IDLE_MS", "0");
    });
}

async fn private_moon(test: &str) -> Option<EphemeralMoon> {
    force_instant_reclaim();
    match EphemeralMoon::spawn().await {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("{test}: no ephemeral Moon ({e}); SKIP");
            None
        }
    }
}

/// Build the pre-fix wreckage by hand: a topic created with the server default
/// `MAXDELIVERY 3`, a burst pushed into it, and one `COUNT 1` pop that claims
/// four and returns one. Returns the raw connection so callers can keep
/// inspecting the same server.
async fn strand_a_backlog(url: &str, scope: &Scope, bodies: &[&str]) -> redis::aio::MultiplexedConnection {
    let host = url.strip_prefix("moon://").unwrap_or(url);
    let client = redis::Client::open(format!("redis://{host}")).expect("redis client");
    let mut conn = client.get_multiplexed_async_connection().await.expect("connect");

    let topic = format!("lunaris:{}:{TOPIC}", scope.as_str());
    let _: String = redis::cmd("MQ")
        .arg("CREATE")
        .arg(&topic)
        .arg("MAXDELIVERY")
        .arg(3)
        .query_async(&mut conn)
        .await
        .expect("MQ CREATE");
    for body in bodies {
        let _: redis::Value = redis::cmd("MQ")
            .arg("PUSH")
            .arg(&topic)
            .arg("body")
            .arg(*body)
            .query_async(&mut conn)
            .await
            .expect("MQ PUSH");
    }
    // The pop that does the damage: claims `1 + 3` and returns 1.
    let _: redis::Value = redis::cmd("MQ")
        .arg("POP")
        .arg(&topic)
        .arg("COUNT")
        .arg(1)
        .query_async(&mut conn)
        .await
        .expect("MQ POP");
    conn
}

/// Vacuity floor, and the reason the other tests mean anything: prove the
/// strand is real — after the damaging pop, `MQ POP` returns nothing at all
/// even though entries remain pending.
#[tokio::test]
async fn the_strand_this_test_builds_is_real() {
    let Some(moon) = private_moon("the_strand_this_test_builds_is_real").await else {
        return;
    };
    let scope = Scope::new("test.mq.strand-floor").unwrap();
    let bodies = ["s1", "s2", "s3", "s4"];
    let mut conn = strand_a_backlog(moon.url(), &scope, &bodies).await;
    let topic = format!("lunaris:{}:{TOPIC}", scope.as_str());

    let again: redis::Value = redis::cmd("MQ")
        .arg("POP")
        .arg(&topic)
        .arg("COUNT")
        .arg(10)
        .query_async(&mut conn)
        .await
        .expect("MQ POP");
    let empty = matches!(&again, redis::Value::Nil)
        || matches!(&again, redis::Value::Array(a) if a.is_empty());
    assert!(empty, "expected the surplus to be unreachable via MQ POP, got {again:?}");

    let pending: redis::Value =
        redis::cmd("XPENDING").arg(&topic).arg("__mq_consumers").query_async(&mut conn).await.expect("XPENDING");
    let count = match &pending {
        redis::Value::Array(a) => match a.first() {
            Some(redis::Value::Int(n)) => *n,
            other => panic!("unexpected XPENDING shape: {other:?}"),
        },
        other => panic!("unexpected XPENDING shape: {other:?}"),
    };
    assert!(count > 0, "nothing was stranded, so the recovery test would prove nothing");
}

/// The finding in one assertion: attach a subscriber to a damaged topic and
/// every message published to it — stranded or not — comes out.
#[tokio::test]
async fn a_subscriber_recovers_messages_stranded_before_the_fix() {
    let Some(moon) = private_moon("a_subscriber_recovers_messages_stranded_before_the_fix").await
    else {
        return;
    };
    let scope = Scope::new("test.mq.strand-recover").unwrap();
    let bodies = ["r1", "r2", "r3", "r4", "r5", "r6"];
    let _conn = strand_a_backlog(moon.url(), &scope, &bodies).await;

    let storage = MoonStorage::connect_with_dim(moon.url(), 768).await.expect("connect");
    let mut stream = storage.subscribe(&scope, GROUP, TOPIC, 0).await.expect("subscribe");

    let mut got: Vec<String> = Vec::new();
    for _ in 0..bodies.len() {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(Ok(msg))) => got.push(String::from_utf8_lossy(&msg.payload).into_owned()),
            other => panic!(
                "delivery stopped after {} of {} messages ({other:?}); got {got:?}",
                got.len(),
                bodies.len()
            ),
        }
    }

    got.sort();
    let mut want: Vec<String> = bodies.iter().map(|s| (*s).to_string()).collect();
    want.sort();
    assert_eq!(got, want, "a damaged topic must give up every message it still holds");
}

/// The other half: recovery must not double-deliver on a healthy topic.
#[tokio::test]
async fn a_healthy_topic_still_delivers_each_message_once() {
    let Some(moon) = private_moon("a_healthy_topic_still_delivers_each_message_once").await else {
        return;
    };
    let storage = MoonStorage::connect_with_dim(moon.url(), 768).await.expect("connect");
    let scope = Scope::new("test.mq.strand-nodup").unwrap();

    let sent: Vec<String> = (1..=4).map(|i| format!("once-{i}")).collect();
    for body in &sent {
        storage.publish(&scope, TOPIC, 0, Bytes::from(body.clone())).await.expect("publish");
    }

    let mut stream = storage.subscribe(&scope, GROUP, TOPIC, 0).await.expect("subscribe");
    let mut got: Vec<String> = Vec::new();
    for _ in 0..sent.len() {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(Ok(msg))) => got.push(String::from_utf8_lossy(&msg.payload).into_owned()),
            other => panic!("delivery stopped after {} of {} ({other:?})", got.len(), sent.len()),
        }
    }
    // Nothing more must arrive: a fifth read within a generous window means
    // the recovery sweep re-delivered something the poll loop already ACKed.
    let extra = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
    assert!(
        extra.is_err(),
        "a healthy topic delivered a duplicate after {got:?}: {extra:?}"
    );

    got.sort();
    let mut want = sent.clone();
    want.sort();
    assert_eq!(got, want);
}
