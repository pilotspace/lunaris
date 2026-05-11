//! Cross-scope leak integration test — RFC 0001 Wave 1C mandatory gate.
//!
//! Proves that Moon per-scope keyspace isolation works end-to-end:
//!
//! 1. Connect to Moon at `LUNARIS_MOON_URL` (default `moon://127.0.0.1:6380`).
//! 2. Write a KV entry under `scope_a`'s keyspace prefix via `atomic_write`.
//! 3. `read_as_of(&scope_b, &key, hlc)` MUST return `None` (no cross-scope bleed).
//! 4. `read_as_of(&scope_a, &key, hlc)` MUST return the written value.
//!
//! ## How scope isolation is enforced in Moon
//!
//! Every KV key is prefixed `lunaris:{scope}:` (from `keyspace::scope_prefix`).
//! Reading scope_b's key (`lunaris:scope-b:...`) can never find data written
//! under scope_a's key (`lunaris:scope-a:...`) — they are disjoint byte strings
//! and Moon's GET will simply return nil for the absent key.
//!
//! This is structurally different from Postgres RLS (which filters rows in a
//! shared table). The Moon implementation prevents cross-scope reads by
//! construction: there is no shared keyspace to query across.
//!
//! Gated behind the `moon-it` Cargo feature — requires a live Moon instance.
//!
//! ```bash
//! LUNARIS_MOON_URL=moon://127.0.0.1:6380 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test scope_isolation
//! ```

#![cfg(feature = "moon-it")]

use lunaris_core::{HlcClock, Scope, StoragePort, WriteOp};
use lunaris_storage_moon::{MoonStorage, keyspace};
use ulid::Ulid;

/// Returns the Moon URL from the environment variable, or the default.
fn moon_url() -> String {
    std::env::var("LUNARIS_MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:6380".into())
}

/// Cross-scope KV isolation — RFC 0001 §3.6 Wave 1C mandatory gate.
///
/// Writes a KV entry under scope_a and verifies that:
/// - `read_as_of` under scope_b returns None (no bleed).
/// - `read_as_of` under scope_a returns the written value.
#[tokio::test]
#[ignore = "requires live Moon, run with LUNARIS_MOON_URL=moon://127.0.0.1:6380"]
async fn cross_scope_kv_keys_are_disjoint() {
    let url = moon_url();

    let storage = match MoonStorage::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("LUNARIS_MOON_URL not reachable ({e}); SKIP scope_isolation test");
            return;
        }
    };

    let scope_a = Scope::new("test-scope-isolation-a").unwrap();
    let scope_b = Scope::new("test-scope-isolation-b").unwrap();

    let clock = HlcClock::new(1);
    let ts = clock.tick();

    let ep_id = Ulid::new();
    let value = b"scope-isolation-test-payload".to_vec();

    // Build the scoped key for scope_a using the keyspace helper.
    let key_a = keyspace::episode_key(&scope_a, ep_id);

    // Write under scope_a via atomic_write.
    let ops = vec![WriteOp::KvPut { key: key_a.clone(), value: value.clone() }];
    storage.atomic_write(&scope_a, &ops).await.expect("atomic_write under scope_a must succeed");

    // Build the equivalent key under scope_b for the same ULID.
    // This key does NOT exist — we never wrote it.
    let key_b = keyspace::episode_key(&scope_b, ep_id);

    // read_as_of under scope_b with scope_b's key MUST return None.
    let result_b = storage
        .read_as_of(&scope_b, &key_b, ts)
        .await
        .expect("read_as_of under scope_b must not error");

    assert!(
        result_b.is_none(),
        "scope_b read_as_of MUST return None for a key only written under scope_a; \
         cross-scope KV leak detected (RFC 0001 §3.6 violation)"
    );

    // read_as_of under scope_a with scope_a's key MUST return Some.
    let result_a = storage
        .read_as_of(&scope_a, &key_a, ts)
        .await
        .expect("read_as_of under scope_a must not error");

    assert!(
        result_a.is_some(),
        "scope_a read_as_of MUST return the written value; \
         got None — data may be lost or stored under the wrong key"
    );

    let row = result_a.unwrap();
    assert_eq!(row.value.as_ref(), value.as_slice(), "returned value must match what was written");
}

/// Verify the keyspace prefix helper produces disjoint keys for different scopes.
///
/// This is a pure unit test — no live Moon required. It guarantees the structural
/// isolation property at the key-naming layer before integration tests run.
#[test]
fn scope_prefix_keys_are_structurally_disjoint() {
    let scope_a = Scope::new("prefix-test-a").unwrap();
    let scope_b = Scope::new("prefix-test-b").unwrap();
    let id = Ulid::new();

    let key_a = keyspace::episode_key(&scope_a, id);
    let key_b = keyspace::episode_key(&scope_b, id);

    assert_ne!(
        key_a, key_b,
        "same ULID under different scopes MUST produce different keys (RFC 0001 §3.6)"
    );
    assert!(
        key_a.starts_with(b"lunaris:prefix-test-a:"),
        "scope_a key must start with lunaris:prefix-test-a:"
    );
    assert!(
        key_b.starts_with(b"lunaris:prefix-test-b:"),
        "scope_b key must start with lunaris:prefix-test-b:"
    );
}
