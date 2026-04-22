//! Phase 11 Plan 11-01 RCPDOC-01 — `DocumentKnowledgeBase` documentary wrapper.
//!
//! Thin composition of [`DocumentCorpus`]. Each method forwards into ≤ 2
//! primitive calls; NO business logic lives here. The wrapper exists to make
//! the "knowledge-base over a document source" recipe discoverable from the
//! public surface per blueprint §7 recipe catalog, and to give Phase 11-02
//! codegen a stable IR symbol to reflect into Py / TS.
//!
//! ## ≤ 30 LOC public-surface contract (RCPDOC-01)
//!
//! Public symbols: `new`, `ingest`, `filter`, `top`, `search`. Five `pub fn`
//! / `pub async fn` declarations — LOC-guard test at the bottom caps at 6
//! (matches [`crate::DocumentCorpus`] template verbatim).
//!
//! ## Scoping
//!
//! `new(mem, source_prefix)` constructs the inner [`DocumentCorpus`] bound to
//! `source_prefix`. Every `search` call scopes through [`Filter::StartsWith`]
//! on the Episode `source` field inside [`DocumentCorpus::search`] — the
//! wrapper does not re-filter.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_retrieve::Hit;

use crate::DocumentCorpus;

/// Documentary knowledge-base wrapper. Owns a [`DocumentCorpus`] bound to a
/// single `source_prefix`. Clone-safe (inner corpus is `Clone`).
#[derive(Clone)]
pub struct DocumentKnowledgeBase {
    corpus: DocumentCorpus,
}

impl DocumentKnowledgeBase {
    /// Construct a knowledge base bound to `source_prefix` (e.g. `"kb:docs/"`).
    pub fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self {
        Self { corpus: DocumentCorpus::new(lunaris, source_prefix) }
    }

    /// Ingest chunked `(content, metadata)` pairs. Forwards to
    /// [`DocumentCorpus::ingest`] (1 primitive call).
    pub async fn ingest(
        &self,
        chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    ) -> Result<(), LunarisError> {
        self.corpus.ingest(chunks).await
    }

    /// Add an equality filter on a metadata field (1 primitive call).
    pub fn filter(self, field: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        Self { corpus: self.corpus.filter(field, value) }
    }

    /// Cap output at `k` hits (1 primitive call).
    pub fn top(self, k: usize) -> Self {
        Self { corpus: self.corpus.top(k) }
    }

    /// Execute the fused Vector + Keyword RRF recall (1 primitive call).
    pub async fn search(self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.corpus.search(query).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn document_knowledge_base_public_surface_under_30_loc() {
        let src = include_str!("./document_knowledge_base.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "RCPDOC-01 ≤30-LOC contract: DocumentKnowledgeBase has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "RCPDOC-01 contract: DocumentKnowledgeBase needs ≥3 public methods; got {pub_fns}"
        );
    }
}
