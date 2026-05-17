//! `list_scopes` end-to-end against a live Moon — pagination, dedupe, prefix.
//!
//! Gated behind the `moon-it` Cargo feature — requires a live Moon instance
//! at `LUNARIS_MOON_URL` (default `moon://127.0.0.1:6380`).
//!
//! ```bash
//! LUNARIS_MOON_URL=moon://127.0.0.1:6380 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test list_scopes
//! ```
//!
//! Each test prefixes its scope strings with `test-list-scopes-<id>` so
//! concurrent runs and pre-existing scopes do not interfere. The prefix
//! filter is then used to scope `list_scopes` calls to this test's own
//! scopes, making the assertions independent of any other data already
//! resident in the Moon instance.

#![cfg(feature = "moon-it")]

use lunaris_core::{Scope, StoragePort, WriteOp, keyspace::episode_key};
use lunaris_storage_moon::MoonStorage;
use ulid::Ulid;

fn moon_url() -> String {
    std::env::var("LUNARIS_MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:6380".into())
}

/// Helper: try to connect; SKIP (return None) if Moon is not reachable.
async fn maybe_connect() -> Option<MoonStorage> {
    match MoonStorage::connect(&moon_url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("LUNARIS_MOON_URL not reachable ({e}); SKIP list_scopes test");
            None
        }
    }
}

/// Seed three scopes under a unique test prefix so concurrent runs don't
/// collide. Returns the prefix used.
async fn seed_three_scopes(storage: &MoonStorage) -> String {
    let prefix = format!("test-list-scopes-{}", Ulid::new());
    for suffix in ["alpha", "beta", "gamma"] {
        let scope_name = format!("{prefix}-{suffix}");
        let scope = Scope::new(&scope_name).expect("valid scope");
        let id = Ulid::new();
        let key = episode_key(&scope, id);
        storage
            .atomic_write(&scope, &[WriteOp::KvPut { key, value: b"x".to_vec() }])
            .await
            .expect("seed write");
    }
    prefix
}

#[tokio::test]
#[ignore = "requires live Moon, run with LUNARIS_MOON_URL=moon://127.0.0.1:6380"]
async fn list_scopes_returns_all_seeded_scopes_under_test_prefix() {
    let Some(storage) = maybe_connect().await else { return };
    let prefix = seed_three_scopes(&storage).await;

    let page = storage.list_scopes(Some(&prefix), 100, None).await.expect("list_scopes");
    let names: Vec<&str> = page.scopes.iter().map(|s| s.as_str()).collect();
    assert_eq!(names.len(), 3, "expected exactly 3 scopes under prefix {prefix}, got {names:?}");
    assert_eq!(names[0], format!("{prefix}-alpha"));
    assert_eq!(names[1], format!("{prefix}-beta"));
    assert_eq!(names[2], format!("{prefix}-gamma"));
    assert!(page.next_cursor.is_none(), "single page with limit=100 must not return cursor");
}

#[tokio::test]
#[ignore = "requires live Moon"]
async fn list_scopes_pagination_cursor_continuity() {
    let Some(storage) = maybe_connect().await else { return };
    let prefix = seed_three_scopes(&storage).await;

    // Page 1 of size 1 under the test prefix.
    let p1 = storage.list_scopes(Some(&prefix), 1, None).await.expect("page 1");
    assert_eq!(p1.scopes.len(), 1);
    assert_eq!(p1.scopes[0].as_str(), format!("{prefix}-alpha"));
    assert!(p1.next_cursor.is_some(), "more available → cursor must be present");

    // Page 2 resumes via cursor.
    let p2 =
        storage.list_scopes(Some(&prefix), 1, p1.next_cursor.as_deref()).await.expect("page 2");
    assert_eq!(p2.scopes.len(), 1);
    assert_eq!(p2.scopes[0].as_str(), format!("{prefix}-beta"));

    // Page 3 — final, cursor should clear.
    let p3 =
        storage.list_scopes(Some(&prefix), 1, p2.next_cursor.as_deref()).await.expect("page 3");
    assert_eq!(p3.scopes.len(), 1);
    assert_eq!(p3.scopes[0].as_str(), format!("{prefix}-gamma"));
    assert!(p3.next_cursor.is_none(), "final page must clear cursor");
}

#[tokio::test]
#[ignore = "requires live Moon"]
async fn list_scopes_dedupe_across_primitive_kinds() {
    use lunaris_core::keyspace::{chunk_key, entity_key, episode_key};
    let Some(storage) = maybe_connect().await else { return };

    // One scope; write three keys of different kinds. list_scopes must
    // collapse them into a single scope entry.
    let prefix = format!("test-list-scopes-dedupe-{}", Ulid::new());
    let scope = Scope::new(&prefix).expect("valid scope");
    storage
        .atomic_write(
            &scope,
            &[
                WriteOp::KvPut { key: episode_key(&scope, Ulid::new()), value: b"a".to_vec() },
                WriteOp::KvPut { key: chunk_key(&scope, Ulid::new()), value: b"b".to_vec() },
                WriteOp::KvPut { key: entity_key(&scope, Ulid::new()), value: b"c".to_vec() },
            ],
        )
        .await
        .expect("seed");

    let page = storage.list_scopes(Some(&prefix), 100, None).await.expect("list_scopes");
    assert_eq!(page.scopes.len(), 1, "three keys, one scope; got {:?}", page.scopes);
    assert_eq!(page.scopes[0].as_str(), prefix);
}

#[tokio::test]
#[ignore = "requires live Moon"]
async fn list_scopes_limit_zero_short_circuits() {
    let Some(storage) = maybe_connect().await else { return };
    let page = storage
        .list_scopes(None, 0, None)
        .await
        .expect("limit=0 must succeed without Moon round-trip");
    assert!(page.scopes.is_empty());
    assert!(page.next_cursor.is_none());
}
