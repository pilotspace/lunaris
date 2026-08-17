//! GA-1 surface pin — the hook's hybrid hot path builds its recall root
//! THROUGH `lunaris_retrieve::production_root(k, true)` (fact legs stay
//! default-ON for the hook, per the hook-recall-graph-hybrid contract), and
//! the opt-in `LUNARIS_RECALL_RERANK` stage is NEVER applied on this
//! latency-critical injection path.
//!
//! RED until `lunaris_hook::context::hook_recall_root` + the
//! `lunaris_retrieve` GA-1 composition surface land (compile-red confined to
//! this test binary — same convention as `context_hybrid_root.rs`).

use lunaris_hook::context::hook_recall_root;
use lunaris_retrieve::{plan_repr, production_root};

#[test]
fn hook_root_is_the_graph_on_production_root() {
    assert_eq!(
        plan_repr(&hook_recall_root(20)),
        plan_repr(&production_root(20, true)),
        "hook hot-path root must be production_root(k, true) — one composition, every surface"
    );
}

#[test]
fn hook_root_never_carries_a_rerank_stage() {
    let plan = plan_repr(&hook_recall_root(20));
    assert!(
        !plan.contains("rerank("),
        "hook context injection is rerank-free regardless of LUNARIS_RECALL_RERANK; got {plan}"
    );
}
