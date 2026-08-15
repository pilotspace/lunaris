//! LIVE validation of the connect-time single-shard guard (0.7.0 task 22).
//!
//! `tests/multishard_failfast.rs` pins the POLICY against scripted RESP fakes.
//! This file pins the PHYSICS: that the co-location probe in
//! [`lunaris_storage_moon::shards::probe_shard_topology`] — the exact function
//! `MoonClient::connect` calls — actually reads a real Moon's shard count.
//!
//! Gated behind the `moon-it` Cargo feature (same convention as
//! `tests/list_scopes.rs`) and driven by two env vars:
//!
//! ```bash
//! # single-shard leg
//! moon --port 6393 --shards 1 --dir "$D" --appendonly no &
//! LUNARIS_MOON_URL=moon://127.0.0.1:6393 LUNARIS_TEST_MOON_SHARDS=1 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test multishard_live
//!
//! # multi-shard leg
//! moon --port 6393 --shards 4 --dir "$D" --appendonly no &
//! LUNARIS_MOON_URL=moon://127.0.0.1:6393 LUNARIS_TEST_MOON_SHARDS=4 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test multishard_live
//! ```
//!
//! `LUNARIS_TEST_MOON_SHARDS` is what the server was STARTED with — the test
//! asserts the probe rediscovers it. Unset ⇒ the tests skip (they cannot know
//! the right answer). The probe dials raw RESP rather than going through
//! `MoonClient::connect` on purpose: this must stay runnable against a Moon
//! older than `MIN_MOON_VERSION`, since detecting the shard shape is orthogonal
//! to the version gate.

#![cfg(feature = "moon-it")]

use lunaris_storage_moon::ShardTopology;
use lunaris_storage_moon::shards;

fn moon_url() -> String {
    std::env::var("LUNARIS_MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:6380".into())
}

/// The shard count the operator started the server with, or `None` to skip.
fn expected_shards() -> Option<u64> {
    std::env::var("LUNARIS_TEST_MOON_SHARDS").ok()?.trim().parse().ok()
}

/// Raw `redis://` connection to whatever `LUNARIS_MOON_URL` points at.
async fn connect_raw() -> Option<redis::aio::MultiplexedConnection> {
    let parsed = url::Url::parse(&moon_url()).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port().unwrap_or(6380);
    let client = redis::Client::open(format!("redis://{host}:{port}")).ok()?;
    match client.get_multiplexed_async_connection().await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("LUNARIS_MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

/// The probe's verdict must match the shard count the server was started with.
#[tokio::test]
async fn probe_rediscovers_the_servers_shard_count() {
    let Some(n) = expected_shards() else {
        eprintln!("LUNARIS_TEST_MOON_SHARDS unset; SKIP");
        return;
    };
    let Some(mut conn) = connect_raw().await else { return };

    let verdict = shards::probe_shard_topology(&mut conn).await.expect("probe transport");

    if n > 1 {
        assert!(
            matches!(verdict, ShardTopology::Multi(_)),
            "a --shards {n} Moon must be detected as multi-shard, got {verdict:?}"
        );
    } else {
        assert!(
            matches!(verdict, ShardTopology::Single(_)),
            "a --shards 1 Moon must be detected as single-shard, got {verdict:?}"
        );
    }
}

/// The verdict must be a property of the SERVER, not of which shard the
/// connection happened to land on — that is the whole reason the probe uses
/// `MULTI/EXEC` (owner-routed, classified up front) rather than `TXN.*`
/// (rejects only what is not local to the connection). Eight fresh connections
/// must all agree.
#[tokio::test]
async fn the_verdict_is_stable_across_fresh_connections() {
    let Some(n) = expected_shards() else {
        eprintln!("LUNARIS_TEST_MOON_SHARDS unset; SKIP");
        return;
    };
    let want_multi = n > 1;
    for attempt in 0..8 {
        let Some(mut conn) = connect_raw().await else { return };
        let verdict = shards::probe_shard_topology(&mut conn).await.expect("probe transport");
        assert_eq!(
            matches!(verdict, ShardTopology::Multi(_)),
            want_multi,
            "connection {attempt} disagreed with the others: {verdict:?}"
        );
    }
}

/// The probe writes NOTHING — no canary key to clean up on any path. Proven by
/// scanning the reserved partition after a probe, on both legs.
#[tokio::test]
async fn the_probe_leaves_no_residue() {
    if expected_shards().is_none() {
        eprintln!("LUNARIS_TEST_MOON_SHARDS unset; SKIP");
        return;
    }
    let Some(mut conn) = connect_raw().await else { return };

    let _ = shards::probe_shard_topology(&mut conn).await.expect("probe transport");

    // Every probe key must still be absent.
    let mut exists = redis::cmd("EXISTS");
    for key in shards::probe_keys() {
        exists.arg(key);
    }
    let hits: i64 = exists.query_async(&mut conn).await.expect("EXISTS");
    assert_eq!(hits, 0, "the probe must not create any of its own keys");

    // And nothing at all under the reserved partition.
    let (_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
        .arg(0)
        .arg("MATCH")
        .arg(format!("lunaris:{}:*", shards::PROBE_SCOPE))
        .arg("COUNT")
        .arg(10_000)
        .query_async(&mut conn)
        .await
        .expect("SCAN");
    assert!(keys.is_empty(), "reserved probe partition must be empty, found {keys:?}");
}
