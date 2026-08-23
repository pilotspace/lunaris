//! Canonical fused-root compositions.
//!
//! KG-RAG wiring Wave B (2026-07-21): [`hybrid_root`] was born in
//! `lunaris-hook` (context.rs, hook-recall-graph-hybrid contract v1.1) as the
//! hook's private fused recall root. Promoted here so the umbrella
//! `Lunaris::recall()` composes the SAME root when the graph pipeline is
//! enabled — one composition, every caller (hook, core recall, HTTP, MCP).
//!
//! KG-RAG facts-as-graph-nodes (2026-07-22): the facts branch's vector leg
//! is [`Navigate`] instead of a flat [`Vector`] KNN. Facts now join the
//! entity graph at ingest (`(subject)-[:HAS_FACT]->(fact)-[:FACT_ABOUT]
//! ->(object)`), so a `Navigate::new("entities", k)` KNN-seeds on entity
//! similarity, then graph-hops to facts connected to those entities —
//! including facts from OTHER sessions that share an entity, which a flat
//! per-session text-similarity search cannot reach. `Keyword::bm25("facts")`
//! stays as the lexical leg (Navigate has no BM25 seed surface). On backends
//! without native graph-navigate, `Navigate` degrades to plain `Vector`
//! search transparently (same behavior the old flat leg had everywhere).
//!
//! GA-1 (2026-08-17): [`production_root`] is THE production recall root.
//! Every production surface — MCP `memory.recall` (+contextd), the hook's
//! context-injection hot path, and HTTP `/v1/recall` / SDK
//! `Lunaris::recall()` — builds its root through this one function (each
//! surface is pinned by a conformance test against [`plan_repr`]). The
//! opt-in cross-encoder stage is composed by [`production_root_reranked`];
//! it is gated per-surface on `LUNARIS_RECALL_RERANK` (default OFF) and the
//! hook hot path NEVER applies it.

use std::sync::Arc;

use lunaris_rerank::Reranker;

use crate::operators::Retriever;
use crate::operators::combinators::{AndRetriever, OrRetriever, ThenRetriever};
use crate::operators::fuse::FuseRrfRetriever;
use crate::operators::keyword::Keyword;
use crate::operators::modifiers::TopRetriever;
use crate::operators::navigate::Navigate;
use crate::operators::rerank::RerankRetriever;
use crate::operators::vector::Vector;

/// The chunks leg: `Vector("chunks",k) ∧ BM25("chunks",k)`.
fn chunks_leg(k: usize) -> AndRetriever {
    Vector::new("chunks", k).and(Keyword::bm25("chunks", k))
}

/// The facts leg: `Navigate("entities",k, fallback "facts") ∧ BM25("facts",k)`.
fn facts_leg(k: usize) -> AndRetriever {
    Navigate::new("entities", k).with_fallback_index("facts").and(Keyword::bm25("facts", k))
}

/// The fused tree behind [`production_root`] — legs chosen by the graph
/// toggle, fused with the workspace-wide RRF constant 60.
fn fused_root(k: usize, graph: bool) -> FuseRrfRetriever {
    if graph {
        AndRetriever::new(Box::new(chunks_leg(k)), Box::new(facts_leg(k))).fuse_rrf(60)
    } else {
        chunks_leg(k).fuse_rrf(60)
    }
}

/// The fused hybrid root: `(Vector ∧ BM25)("chunks") ∧ (Navigate ∧ BM25)("facts")
/// → fuse_rrf(60)`.
///
/// Both facts legs are live signals: `fact_text` is FT-indexed as `content`
/// (BM25 leg) and, since KG-RAG Wave C, graph-ON ingest stores REAL embedder
/// vectors for facts (Navigate's KNN seed — pre-Wave-C rows carry `det_vec`
/// stubs until re-ingested). RRF k=60 matches the workspace-wide fusion
/// constant.
///
/// Downstream hydration must be fact-aware (`hydrate_mixed`) or every fact
/// hit this root retrieves is dropped — `RetrievalBuilder::execute()` has
/// been fact-aware since Wave A.
///
/// GA-1: this is now a thin wrapper over the same leg builders
/// [`production_root`] composes — pinned by
/// `tests/production_root_conformance.rs::hybrid_root_is_a_thin_wrapper_over_production_internals`.
pub fn hybrid_root(k: usize) -> FuseRrfRetriever {
    fused_root(k, true)
}

/// GA-1 — THE unified production recall root.
///
/// - `graph == false`: `Vector("chunks",k) ∧ BM25("chunks",k) → fuse_rrf(60)
///   → top(k)` (the shape MCP `memory.recall` always ran).
/// - `graph == true`: the [`hybrid_root`] leg structure (chunks ∧ facts),
///   fused, then `top(k)`.
///
/// Every production surface builds its recall root through this function;
/// per-surface conformance tests compare `plan_repr` output so a future
/// `with_root` divergence fails a NAMED test instead of drifting silently
/// (the pre-GA-1 bug: `memory.recall` replaced the graph-ON hybrid root with
/// a chunks-only root, discarding fact legs under `LUNARIS_GRAPH_ENABLED=1`).
pub fn production_root(k: usize, graph: bool) -> TopRetriever {
    TopRetriever::new(Box::new(fused_root(k, graph)), k)
}

/// [`production_root`] with the opt-in cross-encoder stage: `fuse_rrf(60) →
/// rerank(top_in) → top(k)`.
///
/// `top_in` is the caller's `LUNARIS_RECALL_RERANK_TOP_IN` override; `None`
/// defaults to `2*k`. Either way the pool is clamped to at least `k` — the
/// rerank pool may never be narrower than the final top-k.
///
/// The `reranker` Arc is the handle's already-resolved LAZY reranker: no
/// model weights load at composition time, only on the first rerank pass of
/// an executed query. Surfaces MUST only call this when their rerank toggle
/// is ON so the OFF path provably never touches the reranker.
pub fn production_root_reranked(
    k: usize,
    graph: bool,
    reranker: Arc<dyn Reranker>,
    top_in: Option<usize>,
) -> TopRetriever {
    let top_in = top_in.unwrap_or_else(|| k.saturating_mul(2)).max(k);
    RerankRetriever::with_top_in(Box::new(fused_root(k, graph)), reranker, top_in).top(k)
}

/// Render a canonical, stable plan string for an operator tree.
///
/// GA-1 conformance surface: per-surface tests compare the plan of the root
/// a surface actually builds against [`production_root`]'s for the same
/// config. The rendering covers every operator that appears in a production
/// root (plus the common combinators); unknown operators render as
/// `<opaque>` so a novel stage is VISIBLE in a failed snapshot rather than
/// silently equal.
pub fn plan_repr(r: &dyn Retriever) -> String {
    let any = r.as_any();
    if let Some(v) = any.downcast_ref::<Vector>() {
        return format!("vector({},k={})", v.index, v.k);
    }
    if let Some(kw) = any.downcast_ref::<Keyword>() {
        return format!("bm25({},k={})", kw.index, kw.k);
    }
    if let Some(n) = any.downcast_ref::<Navigate>() {
        return match n.fallback_index() {
            Some(f) => format!("navigate({},k={},fallback={})", n.index, n.k, f),
            None => format!("navigate({},k={})", n.index, n.k),
        };
    }
    if let Some(g) = any.downcast_ref::<crate::operators::graph::Graph>() {
        // Seed COUNT, not the ids: an EntityId list is unbounded and belongs in
        // a trace, not in a plan string that gets compared for equality. Before
        // F14 `Graph` fell through to `<opaque>` here, which made every graph
        // plan compare equal to every other one.
        return format!("graph(seeds={},hops={})", g.seeds.len(), g.hops);
    }
    if let Some(a) = any.downcast_ref::<AndRetriever>() {
        let (l, rr) = a.branches();
        return format!("and({},{})", plan_repr(l), plan_repr(rr));
    }
    if let Some(o) = any.downcast_ref::<OrRetriever>() {
        return format!("or({},{})", plan_repr(o.left.as_ref()), plan_repr(o.right.as_ref()));
    }
    if let Some(t) = any.downcast_ref::<ThenRetriever>() {
        return format!("then({},{})", plan_repr(t.first.as_ref()), plan_repr(t.second.as_ref()));
    }
    if let Some(f) = any.downcast_ref::<FuseRrfRetriever>() {
        return format!("fuse_rrf(k={},{})", f.k, plan_repr(f.inner_retriever()));
    }
    if let Some(t) = any.downcast_ref::<TopRetriever>() {
        return format!("top(n={},{})", t.n, plan_repr(t.inner.as_ref()));
    }
    if let Some(rk) = any.downcast_ref::<RerankRetriever>() {
        return match rk.min_score {
            Some(ms) => format!(
                "rerank(top_in={},min_score={},{})",
                rk.k_in,
                ms,
                plan_repr(rk.upstream.as_ref())
            ),
            None => format!("rerank(top_in={},{})", rk.k_in, plan_repr(rk.upstream.as_ref())),
        };
    }
    "<opaque>".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operators::Retriever;

    fn downcast_and(r: &dyn Retriever) -> Option<(&dyn Retriever, &dyn Retriever)> {
        r.as_any().downcast_ref::<AndRetriever>().map(|a| (a.left.as_ref(), a.right.as_ref()))
    }

    #[test]
    fn hybrid_root_uses_navigate_for_facts_not_flat_vector() {
        let root = hybrid_root(30);
        let (chunks_and, facts_and) = downcast_and(root.inner.as_ref()).expect("AndRetriever root");

        let (facts_left, _facts_right) = downcast_and(facts_and).expect("facts AndRetriever");
        assert!(
            facts_left.as_any().downcast_ref::<Navigate>().is_some(),
            "facts branch must use Navigate for graph-aware retrieval, not flat Vector"
        );

        let (chunks_left, _chunks_right) = downcast_and(chunks_and).expect("chunks AndRetriever");
        assert!(
            chunks_left.as_any().downcast_ref::<Vector>().is_some(),
            "chunks leg stays plain Vector"
        );
    }

    /// The Navigate seed index is `entities`, but hopping from a seed to a
    /// fact needs native graph-navigate. Anything without it degrades to a
    /// flat vector search, so the leg MUST declare `facts` as its fallback
    /// index — without it the fact vector leg is lost entirely and facts
    /// arrive by BM25 alone. This is live on Moon too: a FILTERED query has
    /// no native navigate surface and takes the same degraded path.
    #[test]
    fn facts_leg_declares_facts_as_its_degraded_fallback_index() {
        let root = hybrid_root(30);
        let (_chunks_and, facts_and) =
            downcast_and(root.inner.as_ref()).expect("AndRetriever root");
        let (facts_left, _) = downcast_and(facts_and).expect("facts AndRetriever");
        let nav = facts_left.as_any().downcast_ref::<Navigate>().expect("facts leg is Navigate");
        assert_eq!(
            nav.fallback_index(),
            Some("facts"),
            "facts leg must fall back to the `facts` index on non-navigate backends"
        );
    }
}
