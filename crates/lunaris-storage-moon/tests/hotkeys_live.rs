//! ADD task `hotkeys-observability` — live HOTKEYS port override (contract
//! FROZEN @ v1, 2026-06-11). Gated behind `moon-it` + `MOON_URL`.
//!
//! Moon samples 1-in-64 keyed commands into a SpaceSaving top-128 sketch
//! (cumulative since process start). We hammer one fresh Lunaris-shaped KV
//! key hard enough (4096 GETs ≈ 64 expected samples) that it must surface
//! in `hot_keys(128)` even with other suites' traffic in the sketch.

#![cfg(feature = "moon-it")]

use lunaris_core::storage::StoragePort;
use lunaris_storage_moon::MoonStorage;
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

/// Pipeline `n` GETs against `key` so Moon's 1-in-64 sampler sees it.
async fn hammer(moon: &MoonStorage, key: &str, n: usize) {
    let mut typed = moon.client().typed();
    let conn = typed.inner_mut();
    for _ in 0..(n / 64) {
        let mut pipe = redis::pipe();
        for _ in 0..64 {
            pipe.cmd("GET").arg(key);
        }
        let _: redis::Value = pipe.query_async(conn).await.expect("pipelined GETs");
    }
}

/// §2 — a hammered Lunaris KV key surfaces through the port override with a
/// positive sampled count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hammered_key_surfaces_in_hot_keys() {
    let Some(moon) = connect_or_skip().await else { return };
    let key = format!("lunaris:hotkeys-it:fact:{}", Ulid::new());

    // SET once so the GETs are real keyed reads, then hammer.
    {
        let mut typed = moon.client().typed();
        let conn = typed.inner_mut();
        let _: redis::Value =
            redis::cmd("SET").arg(&key).arg("v").query_async(conn).await.expect("SET");
    }
    hammer(&moon, &key, 4096).await;

    let hot = moon.hot_keys(128).await.expect("hot_keys against live Moon");
    assert!(!hot.is_empty(), "sketch must not be empty after 4096 keyed reads");
    let mine = hot.iter().find(|hk| hk.key == key.as_bytes());
    let top: Vec<String> = hot
        .iter()
        .take(10)
        .map(|hk| format!("{}={}", String::from_utf8_lossy(&hk.key), hk.sampled_count))
        .collect();
    let mine = mine.unwrap_or_else(|| {
        panic!("hammered key {key} must appear in HOTKEYS reply; top-10: {top:?}")
    });
    assert!(mine.sampled_count >= 1, "sampled_count must be positive, got {}", mine.sampled_count);
}

/// §2 — out-of-range counts are clamped at the Moon boundary; Moon's
/// "COUNT must be an integer between 1 and 128" error never surfaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_is_clamped_to_moon_range() {
    let Some(moon) = connect_or_skip().await else { return };
    let lo = moon.hot_keys(0).await.expect("hot_keys(0) must clamp to 1, not error");
    assert!(lo.len() <= 1, "clamped-to-1 call returns at most one entry");
    let hi = moon.hot_keys(10_000).await.expect("hot_keys(10_000) must clamp to 128, not error");
    assert!(hi.len() <= 128, "clamped-to-128 call returns at most 128 entries");
}
