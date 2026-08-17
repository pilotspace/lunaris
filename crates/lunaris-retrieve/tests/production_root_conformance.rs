//! GA-1 — the unified production recall root (drift-class guard).
//!
//! Before GA-1, the three production surfaces (MCP `memory.recall`, hook
//! context injection, HTTP `/v1/recall` + SDK `Lunaris::recall()`) each built
//! their own recall root and silently diverged: the MCP surface replaced the
//! graph-ON `hybrid_root` with a chunks-only root, dropping fact legs even
//! when `LUNARIS_GRAPH_ENABLED=1`. This suite pins the canonical shapes of
//! [`lunaris_retrieve::production_root`] /
//! [`lunaris_retrieve::production_root_reranked`] via
//! [`lunaris_retrieve::plan_repr`] snapshots so any future re-composition
//! fails a NAMED test instead of drifting silently.
//!
//! RED until `production_root` / `production_root_reranked` / `plan_repr`
//! land in `lunaris_retrieve::composition` (compile-red confined to this
//! test binary — same convention as `context_hybrid_root.rs`).

use std::sync::Arc;

use lunaris_retrieve::{
    NoopReranker, Reranker, hybrid_root, plan_repr, production_root, production_root_reranked,
};

fn noop() -> Arc<dyn Reranker> {
    Arc::new(NoopReranker)
}

/// graph=false → `Vector("chunks",k) ∧ BM25("chunks",k) → fuse_rrf(60) → top(k)`.
/// Exactly the root the MCP surface has always run — now shared by every surface.
#[test]
fn graph_off_shape_is_chunks_hybrid_rrf_top() {
    assert_eq!(
        plan_repr(&production_root(7, false)),
        "top(n=7,fuse_rrf(k=60,and(vector(chunks,k=7),bm25(chunks,k=7))))"
    );
}

/// graph=true → the `hybrid_root` leg structure (chunks ∧ facts), fused, then top(k).
#[test]
fn graph_on_shape_is_hybrid_root_plus_top() {
    assert_eq!(
        plan_repr(&production_root(7, true)),
        "top(n=7,fuse_rrf(k=60,and(\
         and(vector(chunks,k=7),bm25(chunks,k=7)),\
         and(navigate(entities,k=7,fallback=facts),bm25(facts,k=7)))))"
    );
}

/// `hybrid_root` stays the public hook-pinned entry point but must be a thin
/// wrapper over the SAME internals — its plan is exactly the graph-ON
/// production root minus the final `top(k)`.
#[test]
fn hybrid_root_is_a_thin_wrapper_over_production_internals() {
    assert_eq!(
        plan_repr(&production_root(9, true)),
        format!("top(n=9,{})", plan_repr(&hybrid_root(9)))
    );
}

/// Rerank stage sits AFTER fusion, BEFORE the final top(k). Default depth is
/// `2*k` when no override is given.
#[test]
fn reranked_root_inserts_stage_between_fuse_and_top_with_default_depth() {
    assert_eq!(
        plan_repr(&production_root_reranked(10, false, noop(), None)),
        "top(n=10,rerank(top_in=20,fuse_rrf(k=60,and(vector(chunks,k=10),bm25(chunks,k=10)))))"
    );
}

/// `LUNARIS_RECALL_RERANK_TOP_IN` override below k is clamped up to k — the
/// rerank pool may never be narrower than the final top-k.
#[test]
fn reranked_root_clamps_top_in_override_to_at_least_k() {
    let plan = plan_repr(&production_root_reranked(10, false, noop(), Some(5)));
    assert!(
        plan.contains("rerank(top_in=10,"),
        "top_in override 5 < k=10 must clamp to k; got {plan}"
    );
}

/// A wider override passes through untouched.
#[test]
fn reranked_root_honors_wider_top_in_override() {
    let plan = plan_repr(&production_root_reranked(10, true, noop(), Some(50)));
    assert!(plan.contains("rerank(top_in=50,"), "override 50 must pass through; got {plan}");
}

/// The reranked graph-ON root keeps the identical leg structure — the rerank
/// stage wraps the fused tree, it never rebuilds the legs.
#[test]
fn reranked_root_preserves_leg_structure() {
    let plain = plan_repr(&production_root(12, true));
    let reranked = plan_repr(&production_root_reranked(12, true, noop(), None));
    let fused = plan_repr(&hybrid_root(12));
    assert!(plain.contains(&fused), "plain root must contain the fused tree");
    assert!(reranked.contains(&fused), "reranked root must contain the SAME fused tree");
    assert_eq!(reranked, format!("top(n=12,rerank(top_in=24,{fused}))"));
}
