//! Phase 22 — operator-facing optional embedder smoke.
//!
//! Verifies the two contracts the v0.3 default-features flip relies on:
//!
//! 1. `Lunaris::open(url)` succeeds when no embedder feature is
//!    compiled in. The resolver MUST land on `NoopEmbedder` and the handle
//!    MUST come back ready to ingest / query (no panics, no construction
//!    error from "feature missing").
//! 2. `Lunaris::open_with_embedder` plumbs a caller-supplied embedder
//!    through to the handle, including a non-default dim. This is what the
//!    Python / TS SDK `Lunaris.open(url, embedder=cfg)` wrapper relies on.
//!
//! ## Backend (0.7.0 port)
//!
//! The store comes from `lunaris-test-harness` (an ephemeral child-process
//! Moon, degrading to `memory://` where no Moon binary resolves) rather than a
//! hard-coded `memory://`. These tests are about the EMBEDDER RESOLVER, so
//! they must keep calling `Lunaris::open` / `open_with_embedder` themselves —
//! `open_test_engine()` would substitute the harness's `StubEmbedder` and
//! silently destroy what test 1 asserts. Hence `open_test_store()` + the real
//! constructors.

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{Embedder, NOOP_DEFAULT_DIM, NoopEmbedder};
use lunaris_test_harness::open_test_store;

#[tokio::test]
async fn open_memory_succeeds_with_default_resolver() {
    // When the build has no real embedder feature compiled in, the resolver
    // silently falls back to NoopEmbedder at `NOOP_DEFAULT_DIM`. When a real
    // feature IS compiled in, the resolver picks it — we only assert that
    // `open()` succeeds and an embedder is wired (dim > 0).
    let store = open_test_store().await;
    let handle = Lunaris::open(store.url()).await.expect("open must succeed");
    let dim = handle.embedder().dim();
    assert!(dim > 0, "resolved embedder must report a positive dim, got {dim}");
}

#[tokio::test]
async fn open_with_embedder_pins_caller_supplied_dim() {
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(384));
    // 384-d, not 768: Moon fixes its FT index width at FT.CREATE from
    // `embedder.dim()`, and this fixture is the reason the harness threads the
    // caller's embedder through rather than assuming the default.
    let store = open_test_store().await;
    let handle = Lunaris::open_with_embedder(store.url(), embedder)
        .await
        .expect("open_with_embedder must succeed with a custom NoopEmbedder");
    assert_eq!(handle.embedder().dim(), 384);
}

#[tokio::test]
async fn noop_embedder_default_dim_matches_const() {
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::default());
    let store = open_test_store().await;
    let handle = Lunaris::open_with_embedder(store.url(), embedder)
        .await
        .expect("open_with_embedder must succeed with the default NoopEmbedder");
    assert_eq!(handle.embedder().dim(), NOOP_DEFAULT_DIM);
}
