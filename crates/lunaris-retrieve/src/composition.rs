//! Canonical fused-root compositions.
//!
//! KG-RAG wiring Wave B (2026-07-21): [`hybrid_root`] was born in
//! `lunaris-hook` (context.rs, hook-recall-graph-hybrid contract v1.1) as the
//! hook's private fused recall root. Promoted here so the umbrella
//! `Lunaris::recall()` composes the SAME root when the graph pipeline is
//! enabled — one composition, every caller (hook, core recall, HTTP, MCP).

use crate::operators::combinators::AndRetriever;
use crate::operators::fuse::FuseRrfRetriever;
use crate::operators::keyword::Keyword;
use crate::operators::vector::Vector;

/// The fused hybrid root: `(Vector ∧ BM25)("chunks") ∧ (Vector ∧ BM25)("facts")
/// → fuse_rrf(60)`.
///
/// Both facts legs are live signals: `fact_text` is FT-indexed as `content`
/// (BM25 leg) and, since KG-RAG Wave C, graph-ON ingest stores REAL embedder
/// vectors for facts (vector leg — pre-Wave-C rows carry `det_vec` stubs
/// until re-ingested). RRF k=60 matches the workspace-wide fusion constant.
///
/// Downstream hydration must be fact-aware (`hydrate_mixed`) or every fact
/// hit this root retrieves is dropped — `RetrievalBuilder::execute()` has
/// been fact-aware since Wave A.
pub fn hybrid_root(k: usize) -> FuseRrfRetriever {
    let chunks = Vector::new("chunks", k).and(Keyword::bm25("chunks", k));
    let facts = Vector::new("facts", k).and(Keyword::bm25("facts", k));
    AndRetriever::new(Box::new(chunks), Box::new(facts)).fuse_rrf(60)
}
