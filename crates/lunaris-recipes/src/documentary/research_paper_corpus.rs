//! Phase 11 Plan 11-01 RCPDOC-02 — `ResearchPaperCorpus` documentary wrapper.
//!
//! [`DocumentCorpus`] + opt-in citation graph. The graph-on path toggles
//! `Lunaris::graph_pipeline().enable()` (see blueprint §5.2 — graph defaults
//! OFF; opt-in via explicit builder call).
//!
//! ## ≤ 30 LOC public-surface contract (RCPDOC-02)
//!
//! Public symbols: `new`, `with_graph_pipeline`, `ingest`, `search`. Four
//! `pub fn` / `pub async fn` declarations — LOC-guard test caps at 6.
//!
//! ## Graph opt-in semantics
//!
//! `with_graph_pipeline(true)` calls `GraphPipelineHandle::enable`
//! immediately (idempotent; fires once). `with_graph_pipeline(false)` calls
//! `GraphPipelineHandle::disable`. Default remains OFF per blueprint §5.2.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_retrieve::Hit;

use crate::DocumentCorpus;

/// Research-paper corpus wrapper with opt-in citation graph.
#[derive(Clone)]
pub struct ResearchPaperCorpus {
    lunaris: Arc<Lunaris>,
    corpus: DocumentCorpus,
}

impl ResearchPaperCorpus {
    /// Construct bound to `source_prefix` (e.g. `"papers:"`).
    pub fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self {
        let corpus = DocumentCorpus::new(lunaris.clone(), source_prefix);
        Self { lunaris, corpus }
    }

    /// Opt-in the citation graph (1 primitive call on
    /// `GraphPipelineHandle`). Default OFF per blueprint §5.2.
    pub fn with_graph_pipeline(self, on: bool) -> Self {
        let handle = self.lunaris.graph_pipeline();
        if on {
            handle.enable();
        } else {
            handle.disable();
        }
        self
    }

    /// Ingest chunked `(content, metadata)` pairs (1 primitive call).
    pub async fn ingest(
        &self,
        chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    ) -> Result<(), LunarisError> {
        self.corpus.ingest(chunks).await
    }

    /// Execute the RRF-fused recall (1 primitive call).
    pub async fn search(self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.corpus.search(query).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn research_paper_corpus_public_surface_under_30_loc() {
        let src = include_str!("./research_paper_corpus.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "RCPDOC-02 ≤30-LOC contract: ResearchPaperCorpus has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "RCPDOC-02 contract: ResearchPaperCorpus needs ≥3 public methods; got {pub_fns}"
        );
    }
}
