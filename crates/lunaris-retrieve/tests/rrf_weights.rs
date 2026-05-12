//! P0 #2 — `FuseRrfRetriever::with_weights(...)` integration test.
//!
//! Sister file to `rrf_routing.rs`. Builds the canonical
//! `Vector::new(...).and(Keyword::bm25(...)).fuse_rrf(k).with_weights(...)`
//! tree, then downcasts the root `Retriever` back to a `FuseRrfRetriever`
//! and asserts the per-branch weights are stored on the operator. This
//! is the public-API contract that downstream consumers (Helios, the
//! Python/TS SDKs) rely on: the builder method must propagate weights
//! to the operator, not silently drop them.
//!
//! No live Moon listener required — the assertion is structural, on the
//! operator graph itself. End-to-end weighted-RRF math is covered by the
//! `operators::fuse::tests` unit tests.

use std::collections::HashMap;

use lunaris_retrieve::operators::Retriever;
use lunaris_retrieve::{FuseRrfRetriever, Keyword, SourceOp, Vector};

#[test]
fn with_weights_propagates_to_fuse_rrf_node() {
    let weights = HashMap::from([
        (SourceOp::Vector, 1.0_f32),
        (SourceOp::Keyword, 0.5_f32),
    ]);

    let fused = Vector::new("chunks", 30)
        .and(Keyword::bm25("chunks", 30))
        .fuse_rrf(60)
        .with_weights(weights.clone());

    // Downcast through the `Retriever::as_any` escape hatch to verify the
    // weights actually landed on the node — the builder must NOT silently
    // drop them. This mirrors the introspection mechanism used in
    // `fusion::inspect_branches` for routing decisions.
    let any = (&fused as &dyn Retriever).as_any();
    let node = any
        .downcast_ref::<FuseRrfRetriever>()
        .expect("root is FuseRrfRetriever");

    assert_eq!(node.weights().len(), 2, "both weights present");
    assert!((node.weights().get(&SourceOp::Vector).copied().unwrap_or(0.0) - 1.0).abs() < 1e-6);
    assert!((node.weights().get(&SourceOp::Keyword).copied().unwrap_or(0.0) - 0.5).abs() < 1e-6);
}

#[test]
fn fuse_rrf_default_weights_are_empty() {
    // No `.with_weights(...)` call → empty map → preserves legacy
    // behaviour (all-1.0 on client-side, [0.5, 0.5] on Moon-native).
    let fused = Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60);
    let any = (&fused as &dyn Retriever).as_any();
    let node = any.downcast_ref::<FuseRrfRetriever>().expect("root is FuseRrfRetriever");
    assert!(node.weights().is_empty(), "default weights map MUST be empty");
}

#[test]
fn with_weights_overwrites_previous_call() {
    // Builder semantics: the LAST `.with_weights(...)` wins. Users who
    // re-tune the operator down the chain shouldn't end up with merged
    // weights from earlier calls.
    let first = HashMap::from([(SourceOp::Vector, 2.0_f32)]);
    let second = HashMap::from([(SourceOp::Keyword, 3.0_f32)]);

    let fused = Vector::new("chunks", 30)
        .and(Keyword::bm25("chunks", 30))
        .fuse_rrf(60)
        .with_weights(first)
        .with_weights(second.clone());

    let any = (&fused as &dyn Retriever).as_any();
    let node = any.downcast_ref::<FuseRrfRetriever>().expect("root is FuseRrfRetriever");
    assert_eq!(node.weights().len(), 1);
    assert!(node.weights().get(&SourceOp::Vector).is_none(), "first call must be discarded");
    assert!((node.weights().get(&SourceOp::Keyword).copied().unwrap_or(0.0) - 3.0).abs() < 1e-6);
}
