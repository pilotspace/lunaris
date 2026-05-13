//! Phase 22 — operator-facing optional embedder smoke.
//!
//! Verifies the two contracts the v0.3 default-features flip relies on:
//!
//! 1. `Lunaris::open("memory://")` succeeds when no embedder feature is
//!    compiled in. The resolver MUST land on `NoopEmbedder` and the handle
//!    MUST come back ready to ingest / query (no panics, no construction
//!    error from "feature missing").
//! 2. `Lunaris::open_with_embedder` plumbs a caller-supplied embedder
//!    through to the handle, including a non-default dim. This is what the
//!    Python / TS SDK `Lunaris.open(url, embedder=cfg)` wrapper relies on.
//!
//! Both tests use the in-process `memory://` backend (Plan 04 / phase 14.3
//! reuse pattern) so they avoid Moon / Postgres and run on every CI node.

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::Embedder;
use lunaris_embed::{NOOP_DEFAULT_DIM, NoopEmbedder};

#[tokio::test]
async fn open_memory_succeeds_with_default_resolver() {
    // When the build has no real embedder feature compiled in, the resolver
    // silently falls back to NoopEmbedder at `NOOP_DEFAULT_DIM`. When a real
    // feature IS compiled in, the resolver picks it — we only assert that
    // `open()` succeeds and an embedder is wired (dim > 0).
    let handle = Lunaris::open("memory://").await.expect("open memory:// must succeed");
    let dim = handle.embedder().dim();
    assert!(dim > 0, "resolved embedder must report a positive dim, got {dim}");
}

#[tokio::test]
async fn open_with_embedder_pins_caller_supplied_dim() {
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(384));
    let handle = Lunaris::open_with_embedder("memory://", embedder)
        .await
        .expect("open_with_embedder must succeed with a custom NoopEmbedder");
    assert_eq!(handle.embedder().dim(), 384);
}

#[tokio::test]
async fn noop_embedder_default_dim_matches_const() {
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::default());
    let handle = Lunaris::open_with_embedder("memory://", embedder)
        .await
        .expect("open_with_embedder must succeed with the default NoopEmbedder");
    assert_eq!(handle.embedder().dim(), NOOP_DEFAULT_DIM);
}
