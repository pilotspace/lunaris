//! N-04 D2 — `Lunaris::try_with_embedder` hard-fails on dim mismatch.
//!
//! `with_embedder()` keeps the historical infallible swap (KEEP per N-04
//! brief), but the parallel `try_with_embedder()` rejects swaps whose
//! `embedder.dim()` doesn't match the handle's current (storage-provisioned)
//! dim. Moon's `FT.CREATE` and Postgres's `pgvector` column are sized at
//! `Lunaris::open*` time — a post-open swap with a wider/narrower embedder
//! produces garbage similarity scores until the index is rebuilt, so we
//! refuse at the API boundary rather than corrupting recall silently.

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{Embedder, LunarisError, NoopEmbedder, StorageError};

#[tokio::test]
async fn try_with_embedder_accepts_matching_dim() {
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let handle = Lunaris::open_with_embedder("memory://", embedder)
        .await
        .expect("open_with_embedder must succeed");

    let swap: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let handle = handle
        .try_with_embedder(swap)
        .expect("matching dim must Ok");

    assert_eq!(handle.embedder().dim(), 768);
}

#[tokio::test]
async fn try_with_embedder_rejects_mismatched_dim() {
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let handle = Lunaris::open_with_embedder("memory://", embedder)
        .await
        .expect("open_with_embedder must succeed");

    let bad_swap: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(512));
    let err = handle
        .try_with_embedder(bad_swap)
        .expect_err("mismatched dim must Err");

    match err {
        LunarisError::Storage(StorageError::Backend(msg)) => {
            assert!(
                msg.contains("512") && msg.contains("768"),
                "error must name both dims; got: {msg}"
            );
            assert!(
                msg.contains("dim") || msg.contains("dimension"),
                "error must be descriptive; got: {msg}"
            );
        }
        other => panic!("expected Storage(Backend(_)), got {other:?}"),
    }
}
