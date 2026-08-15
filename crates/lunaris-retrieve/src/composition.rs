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

use crate::operators::combinators::AndRetriever;
use crate::operators::fuse::FuseRrfRetriever;
use crate::operators::keyword::Keyword;
use crate::operators::navigate::Navigate;
use crate::operators::vector::Vector;

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
pub fn hybrid_root(k: usize) -> FuseRrfRetriever {
    let chunks = Vector::new("chunks", k).and(Keyword::bm25("chunks", k));
    let facts =
        Navigate::new("entities", k).with_fallback_index("facts").and(Keyword::bm25("facts", k));
    AndRetriever::new(Box::new(chunks), Box::new(facts)).fuse_rrf(60)
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
