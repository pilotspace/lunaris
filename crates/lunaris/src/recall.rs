//! `Lunaris::recall` — the public Phase 2 retrieve entry point.
//!
//! Returns a [`RetrievalBuilder`] seeded with this handle's storage,
//! keyword, embedder, and (when present) the typed `Arc<MoonStorage>`
//! that lets `fuse_rrf` take the Moon-native one-round-trip path.

use lunaris_retrieve::RetrievalBuilder;

use crate::handle::Lunaris;

impl Lunaris {
    /// Build a [`RetrievalBuilder`] bound to this handle's storage, keyword,
    /// embedder, and (when available) the typed `Arc<MoonStorage>` that
    /// enables the Phase 1.5 RRF Moon-native dispatch.
    ///
    /// Default root operator is `Vector::new("chunks", 30)`. Callers replace
    /// the root via [`RetrievalBuilder::with_root`] for the canonical example
    /// from blueprint §8:
    ///
    /// ```ignore
    /// let hits = lunaris.recall()
    ///     .with_root(Vector::new("chunks", 30)
    ///         .and(Keyword::bm25("chunks", 30))
    ///         .fuse_rrf(60)
    ///         .top(5))
    ///     .filter_str("source LIKE 'helios:fs/%'").unwrap()
    ///     .execute(Query::text("brown fox"))
    ///     .await?;
    /// ```
    ///
    /// Plan 03-03 graph-aware extension — once `handle.graph_pipeline().enable()`
    /// is called and Episodes have been ingested with extracted Entities,
    /// callers can compose `Graph::anchored` into the same DSL:
    ///
    /// ```ignore
    /// use lunaris::{EntityId, Graph, Query, Vector};
    /// let alice = EntityId::from_name_and_type("Alice", "Person");
    /// let hits = lunaris.recall()
    ///     .with_root(Vector::new("chunks", 30)
    ///         .and(Graph::anchored(vec![alice], 2))
    ///         .fuse_rrf(60)
    ///         .rerank(lunaris.reranker())
    ///         .top(5))
    ///     .execute(Query::text("Tell me about Alice"))
    ///     .await?;
    /// ```
    pub fn recall(&self) -> RetrievalBuilder {
        let mut b = RetrievalBuilder::from_handle(self.storage(), self.keyword(), self.embedder());
        if let Some(moon) = self.moon_storage() {
            b = b.with_moon_storage(moon);
        }
        b
    }
}
