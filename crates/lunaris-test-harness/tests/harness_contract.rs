//! Contract pins for the ephemeral-Moon test harness.
//!
//! Every Moon-dependent assertion below is gated on a resolvable binary and
//! prints a skip line when there is none — that IS the fallback contract:
//! `cargo test --workspace` must be green on a machine that never built the
//! Moon submodule. CI closes the hole by setting `LUNARIS_TEST_BACKEND=moon`,
//! which turns a missing binary into a panic instead of a skip.

#![forbid(unsafe_code)]

use std::net::TcpStream;
use std::time::Duration;

use lunaris::EpisodeBuilder;
use lunaris_core::Scope;
use lunaris_retrieve::Query;
use lunaris_test_harness::{
    Backend, EphemeralMoon, Policy, RESERVED_PORTS, moon_binary, open_test_engine_with,
    open_test_storage_with, open_test_store_with,
};
use ulid::Ulid;

/// `None` (with a skip line) when this machine has no Moon binary.
fn require_binary(test: &str) -> Option<()> {
    if moon_binary().is_some() {
        Some(())
    } else {
        eprintln!("skipping {test}: no moon binary (set MOON_TEST_BINARY)");
        None
    }
}

fn stub() -> std::sync::Arc<dyn lunaris_core::Embedder> {
    std::sync::Arc::new(lunaris_core::StubEmbedder::new(768))
}

fn scope(tag: &str) -> Scope {
    Scope::new(format!("harness-{tag}-{}", Ulid::new().to_string().to_lowercase())).unwrap()
}

/// `Policy::ForceMemory` never spawns a process, on any machine.
#[tokio::test]
async fn force_memory_yields_the_embedded_backend() {
    let store = open_test_store_with(Policy::ForceMemory).await;
    assert_eq!(store.backend(), Backend::Memory);
    assert_eq!(store.url(), "memory://");
    assert!(store.moon().is_none(), "ForceMemory must not own a Moon process");
}

/// The headline behaviour: with a binary present, `Auto` gives you real Moon.
#[tokio::test]
async fn auto_selects_moon_when_a_binary_is_available() {
    let Some(()) = require_binary("auto_selects_moon_when_a_binary_is_available") else {
        return;
    };
    let store = open_test_store_with(Policy::Auto).await;
    assert_eq!(store.backend(), Backend::Moon, "Auto must prefer Moon when a binary exists");
    assert!(
        store.url().starts_with("moon://127.0.0.1:"),
        "expected a loopback moon:// URL, got {}",
        store.url()
    );
}

/// Never 6379/6380/6381/6399 — 6381 is the maintainer's live memory store and
/// 6399 the dedicated bench Moon.
#[tokio::test]
async fn ephemeral_moon_never_binds_a_reserved_port() {
    let Some(()) = require_binary("ephemeral_moon_never_binds_a_reserved_port") else {
        return;
    };
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
    let Some(()) = require_binary("dropping_the_fixture_reaps_the_process_and_the_data_dir") else {
        return;
    };
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

/// Two fixtures are independent stores. This is the property that makes an
/// ephemeral Moon a faithful replacement for `memory://`, whose every
/// `connect` is a fresh, process-private database.
///
/// Both fixtures are written to before the cross-read. That is not padding:
/// recall against a Moon that has NEVER been written to fails with
/// `no temporal snapshot registered for the given AS_OF timestamp`, where the
/// embedded backend returns an empty result set. Porting a test whose
/// assertion is "recall finds nothing" therefore needs the store seeded with
/// something irrelevant first — see docs/testing/memory-to-moon-port-plan.md.
#[tokio::test]
async fn two_fixtures_do_not_share_state() {
    let Some(()) = require_binary("two_fixtures_do_not_share_state") else {
        return;
    };
    let sc = scope("isolation");

    let a = open_test_engine_with(Policy::RequireMoon, stub()).await;
    let b = open_test_engine_with(Policy::RequireMoon, stub()).await;
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

/// The bare-`StoragePort` seam, for the test files that call
/// `EmbeddedStorage::connect("memory://")` directly instead of going through
/// the engine. Capabilities are the discriminator: Moon reports a native graph
/// and queue, the embedded backend reports neither.
#[tokio::test]
async fn storage_seam_yields_a_live_backend_on_both_paths() {
    let mem = open_test_storage_with(Policy::ForceMemory, 768).await;
    assert_eq!(mem.backend(), Backend::Memory);
    let caps = mem.port().capabilities();
    assert!(!caps.graph_native, "the embedded backend has no native graph");

    let Some(()) = require_binary("storage_seam_yields_a_live_backend_on_both_paths") else {
        return;
    };
    let moon = open_test_storage_with(Policy::RequireMoon, 768).await;
    assert_eq!(moon.backend(), Backend::Moon);
    assert!(
        moon.port().capabilities().graph_native,
        "an ephemeral Moon must report the native graph the embedded backend lacks"
    );
}

/// End-to-end proof the harness hands back a working engine: ingest, then
/// recall the same episode back out of a real Moon.
#[tokio::test]
async fn moon_backed_engine_round_trips_ingest_and_recall() {
    let Some(()) = require_binary("moon_backed_engine_round_trips_ingest_and_recall") else {
        return;
    };
    let engine = open_test_engine_with(Policy::RequireMoon, stub()).await;
    assert_eq!(engine.backend(), Backend::Moon);

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
