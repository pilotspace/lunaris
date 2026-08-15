//! `Lunaris::list_scopes` — high-level pass-through to
//! `StoragePort::list_scopes`, validated against a real backend.
//!
//! The store comes from `lunaris-test-harness` (0.7.0 port off `memory://`):
//! an ephemeral child-process Moon, degrading to the embedded backend where no
//! Moon binary resolves. Either way it is a real `StoragePort` implementation
//! of `list_scopes`, so the test exercises the full path from
//! `Lunaris::list_scopes` through the trait pass-through to the backend impl.

use lunaris::Lunaris;
use lunaris_core::{Scope, WriteOp, keyspace::episode_key};
use lunaris_test_harness::{TestStorage, open_test_storage};
use ulid::Ulid;

/// Helper — open a harness-backed Lunaris handle, bypassing the real embedder
/// resolution path which downloads model weights.
///
/// Returns the [`TestStorage`] guard alongside the handle: it owns the Moon
/// child process, and dropping it would take the backend with it. `NoopEmbedder`
/// defaults to `NOOP_DEFAULT_DIM` (768), which is the harness's storage width,
/// so the two agree without an explicit dim.
async fn open_mem() -> (Lunaris, TestStorage) {
    use std::sync::Arc;

    use lunaris_core::{HlcClock, NoopEmbedder};

    let storage = open_test_storage().await;
    let engine =
        Lunaris::with_parts(storage.port(), Arc::new(NoopEmbedder::default()), HlcClock::new(0));
    (engine, storage)
}

#[tokio::test]
async fn list_scopes_passes_through_to_storage_for_embedded_backend() {
    let (engine, _storage) = open_mem().await;

    // Seed two scopes via direct storage writes (we cannot ingest without an
    // embedder + extractor, but list_scopes only needs the keyspace shape).
    for name in ["passthrough-a", "passthrough-b"] {
        let scope = Scope::new(name).expect("valid scope");
        let key = episode_key(&scope, Ulid::new());
        engine
            .storage()
            .atomic_write(&scope, &[WriteOp::KvPut { key, value: b"x".to_vec() }])
            .await
            .expect("seed");
    }

    let page = engine.list_scopes(Some("passthrough-"), 100, None).await.expect("list_scopes");
    let names: Vec<&str> = page.scopes.iter().map(|s| s.as_str()).collect();
    assert_eq!(names, vec!["passthrough-a", "passthrough-b"]);
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn list_scopes_pagination_round_trips_through_high_level() {
    let (engine, _storage) = open_mem().await;
    for name in ["round-1", "round-2", "round-3"] {
        let scope = Scope::new(name).expect("valid scope");
        let key = episode_key(&scope, Ulid::new());
        engine
            .storage()
            .atomic_write(&scope, &[WriteOp::KvPut { key, value: b"v".to_vec() }])
            .await
            .expect("seed");
    }

    let p1 = engine.list_scopes(Some("round-"), 1, None).await.expect("p1");
    assert_eq!(p1.scopes.len(), 1);
    assert_eq!(p1.scopes[0].as_str(), "round-1");
    let c1 = p1.next_cursor.expect("cursor");

    let p2 = engine.list_scopes(Some("round-"), 1, Some(&c1)).await.expect("p2");
    assert_eq!(p2.scopes[0].as_str(), "round-2");

    let p3 = engine.list_scopes(Some("round-"), 1, p2.next_cursor.as_deref()).await.expect("p3");
    assert_eq!(p3.scopes[0].as_str(), "round-3");
    assert!(p3.next_cursor.is_none(), "last page must clear cursor");
}
