//! Contract pins for the ephemeral-Moon test harness.
//!
//! Through 0.6.x every assertion here was gated on a resolvable binary and
//! printed a skip line when there was none — the fallback contract was
//! "`cargo test --workspace` is green on a machine that never built the Moon
//! submodule". 0.7.0 deleted the `memory://` backend that fallback ran on, so
//! the contract inverted: a machine with no Moon binary FAILS, loudly, with
//! build instructions. The gates are gone with it; a skip here would be the
//! same silent hole the deletion exists to close.

#![forbid(unsafe_code)]

use std::net::TcpStream;
use std::time::Duration;

use lunaris::EpisodeBuilder;
use lunaris_core::Scope;
use lunaris_retrieve::Query;
use lunaris_test_harness::{
    EphemeralMoon, MOON_BINARY_ENV, RESERVED_PORTS, check_backend_env_value, moon_binary,
    open_test_engine_with_embedder, open_test_storage_with_dim, open_test_store,
};
use ulid::Ulid;

fn stub() -> std::sync::Arc<dyn lunaris_core::Embedder> {
    std::sync::Arc::new(lunaris_core::StubEmbedder::new(768))
}

fn scope(tag: &str) -> Scope {
    Scope::new(format!("harness-{tag}-{}", Ulid::new().to_string().to_lowercase())).unwrap()
}

/// The 0.7.0 contract in one assertion: there is no second substrate to pick,
/// so a request for the deleted one is an error rather than a quiet upgrade.
#[test]
fn the_memory_backend_cannot_be_asked_for() {
    let err = check_backend_env_value(Some("memory")).expect_err("memory must be rejected");
    assert!(err.contains("removed in 0.7.0"), "{err}");
    assert!(err.contains(MOON_BINARY_ENV), "{err}");
}

/// Resolution is not conditional any more: `open_test_store` either hands back
/// a real loopback Moon or panics.
#[tokio::test]
async fn every_store_is_a_real_loopback_moon() {
    let store = open_test_store().await;
    assert!(
        store.url().starts_with("moon://127.0.0.1:"),
        "expected a loopback moon:// URL, got {}",
        store.url()
    );
    assert!(!RESERVED_PORTS.contains(&store.moon().port()));
}

/// A missing binary must be diagnosable from the harness alone. We cannot make
/// one disappear mid-suite (env mutation is `unsafe` in edition 2024), so pin
/// the resolver's contract instead: whatever this run resolved, the suite is
/// running against a binary, and the *absence* branch is the panic path proven
/// by `no_moon_message_carries_the_fix` in the crate's unit tests.
#[test]
fn the_suite_runs_against_a_resolved_binary() {
    assert!(
        moon_binary().is_some(),
        "no moon binary resolved — this suite has no fallback since 0.7.0; \
         set ${MOON_BINARY_ENV} or build vendor/moon/target/release/moon"
    );
}

/// Never 6379/6380/6381/6399 — 6381 is the maintainer's live memory store and
/// 6399 the dedicated bench Moon.
#[tokio::test]
async fn ephemeral_moon_never_binds_a_reserved_port() {
    let moon = EphemeralMoon::spawn().await.expect("spawn ephemeral moon");
    assert!(
        !RESERVED_PORTS.contains(&moon.port()),
        "ephemeral Moon bound reserved port {}",
        moon.port()
    );
    assert!(
        moon.data_dir().starts_with(std::env::temp_dir()),
        "data dir must live under temp_dir, got {}",
        moon.data_dir().display()
    );
}

/// Dropping the fixture reaps the process and deletes the scratch directory.
/// Without this, a 60-file port would leave hundreds of orphaned servers.
#[tokio::test]
async fn dropping_the_fixture_reaps_the_process_and_the_data_dir() {
    let moon = EphemeralMoon::spawn().await.expect("spawn ephemeral moon");
    let port = moon.port();
    let dir = moon.data_dir().to_path_buf();
    assert!(dir.is_dir(), "data dir must exist while the fixture is alive");
    assert!(
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(500)
        )
        .is_ok(),
        "moon must be reachable while the fixture is alive"
    );

    drop(moon);

    // The kill is synchronous but the kernel needs a moment to release the
    // listener; poll rather than race it.
    let mut reachable = true;
    for _ in 0..100 {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(100),
        )
        .is_err()
        {
            reachable = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!reachable, "port {port} still accepting after drop — process leaked");
    assert!(!dir.exists(), "scratch dir {} survived drop", dir.display());
}

/// Two fixtures are independent stores. This is the property that made an
/// ephemeral Moon a faithful replacement for `memory://`, whose every
/// `connect` was a fresh, process-private database.
///
/// Both fixtures are written to before the cross-read. That is not padding:
/// recall against a Moon that has NEVER been written to fails with
/// `no temporal snapshot registered for the given AS_OF timestamp`, where the
/// embedded backend returned an empty result set. Porting a test whose
/// assertion is "recall finds nothing" therefore needs the store seeded with
/// something irrelevant first — see docs/testing/memory-to-moon-port-plan.md.
#[tokio::test]
async fn two_fixtures_do_not_share_state() {
    let sc = scope("isolation");

    let a = open_test_engine_with_embedder(stub()).await;
    let b = open_test_engine_with_embedder(stub()).await;
    assert_ne!(a.url(), b.url(), "two fixtures must be distinct servers");

    let secret = Ulid::new();
    a.scoped(sc.clone())
        .ingest(
            EpisodeBuilder::new("iso/one", "the cobalt beacon flashes every 17 seconds").id(secret),
        )
        .await
        .expect("ingest into fixture A");
    b.scoped(sc.clone())
        .ingest(EpisodeBuilder::new("iso/two", "unrelated filler so B has a snapshot"))
        .await
        .expect("ingest into fixture B");

    let hits =
        b.scoped(sc).recall(Query::text("cobalt beacon")).await.expect("recall from fixture B");
    let leaked = hits.iter().any(|h| h.episode_id == secret.to_bytes().to_vec());
    assert!(!leaked, "fixture B saw fixture A's episode: {hits:?}");
}

/// The bare-`StoragePort` seam, for the test files that open a backend directly
/// instead of going through the engine. `graph_native` is the discriminator
/// that proves it is really Moon and not a stub: the deleted embedded backend
/// reported `false` here.
#[tokio::test]
async fn storage_seam_yields_a_live_moon_backend() {
    let storage = open_test_storage_with_dim(768).await;
    let caps = storage.port().capabilities();
    assert!(caps.graph_native, "an ephemeral Moon must report the native graph");
    assert!(caps.queue_native, "an ephemeral Moon must report the native queue");
}

/// End-to-end proof the harness hands back a working engine: ingest, then
/// recall the same episode back out of a real Moon.
#[tokio::test]
async fn moon_backed_engine_round_trips_ingest_and_recall() {
    let engine = open_test_engine_with_embedder(stub()).await;
    assert!(engine.url().starts_with("moon://"));

    let sc = scope("roundtrip");
    let scoped = engine.scoped(sc);
    scoped
        .ingest(EpisodeBuilder::new(
            "roundtrip/lesson",
            "when injection goes quiet check the contextd process age and socket first",
        ))
        .await
        .expect("ingest must succeed against ephemeral Moon");

    let hits = scoped.recall(Query::text("contextd injection quiet")).await.expect("recall");
    assert!(!hits.is_empty(), "ephemeral Moon must serve back what was just ingested");
}
