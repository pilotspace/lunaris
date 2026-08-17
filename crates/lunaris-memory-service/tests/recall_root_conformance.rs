//! GA-1 surface pin — `memory.recall` (MCP + contextd) builds its recall
//! root THROUGH `lunaris_retrieve::production_root`.
//!
//! The pre-GA-1 handler built a private chunks-only root and passed it to
//! `scoped.dsl().with_root(..)`, silently REPLACING the graph-ON
//! `hybrid_root` — fact legs were discarded even with
//! `LUNARIS_GRAPH_ENABLED=1`. This suite pins the surface's root-construction
//! seam (`lunaris_memory_service::recall::recall_root`) to the canonical
//! composition for every (graph, rerank) combination so a future
//! `with_root` divergence fails a NAMED test.
//!
//! RED until the seam + `production_root` land (compile-red confined to this
//! test binary).

use std::sync::Arc;

use lunaris_memory_service::recall::recall_root;
use lunaris_retrieve::{
    NoopReranker, Reranker, plan_repr, production_root, production_root_reranked,
};

fn noop() -> Arc<dyn Reranker> {
    Arc::new(NoopReranker)
}

#[test]
fn graph_off_root_is_production_root() {
    assert_eq!(
        plan_repr(recall_root(40, false, None).as_ref()),
        plan_repr(&production_root(40, false)),
    );
}

#[test]
fn graph_on_root_is_production_root_with_fact_legs() {
    let plan = plan_repr(recall_root(40, true, None).as_ref());
    assert_eq!(plan, plan_repr(&production_root(40, true)));
    assert!(
        plan.contains("navigate(entities,k=40,fallback=facts)"),
        "graph-ON memory.recall must carry the fact legs (the GA-1 bug); got {plan}"
    );
}

#[test]
fn rerank_on_root_is_production_root_reranked() {
    assert_eq!(
        plan_repr(recall_root(40, false, Some((noop(), None))).as_ref()),
        plan_repr(&production_root_reranked(40, false, noop(), None)),
    );
}

#[test]
fn rerank_on_graph_on_root_carries_both_stages() {
    let plan = plan_repr(recall_root(40, true, Some((noop(), Some(64)))).as_ref());
    assert_eq!(plan, plan_repr(&production_root_reranked(40, true, noop(), Some(64))));
    assert!(plan.contains("rerank(top_in=64,"), "explicit top_in must pass through; got {plan}");
    assert!(plan.contains("fallback=facts"), "fact legs must survive the rerank wrap; got {plan}");
}
