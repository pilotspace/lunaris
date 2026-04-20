//! `RetrievalBuilder` — the umbrella API surface that `Lunaris::recall()` returns.
//!
//! Wires the operator root + the four required Arcs (storage, embedder,
//! keyword, clock-substitute via stored Hlc) into a `tower::Service`-shaped
//! retriever. Terminal `.execute(query)` runs the tree and hydrates.
//!
//! ## Pattern — Vector + Keyword (Phase 2)
//!
//! ```ignore
//! let hits = lunaris.recall()
//!     .with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))
//!     .filter_str("source LIKE 'helios:fs/'").unwrap()
//!     .execute(Query::text("brown fox"))
//!     .await?;
//! ```
//!
//! ## Pattern — Graph-anchored recall (Plan 03-02)
//!
//! The canonical blueprint §8 example composes a Vector branch with a
//! Graph branch via the same `fuse_rrf` operator. The Graph branch takes
//! pre-resolved [`crate::EntityId`]s (from the RETRIEVE-13 planner stub
//! per D-13) and walks the graph for `hops` BFS steps:
//!
//! ```ignore
//! let hits = lunaris.recall()
//!     .with_root(
//!         Vector::new("chunks", 30)
//!             .and(Graph::anchored(query_entity_ids, 2))
//!             .fuse_rrf(60)
//!             .rerank(handle.reranker())
//!             .top(5))
//!     .execute(Query::text("brown fox"))
//!     .await?;
//! ```
//!
//! `Graph::anchored(_, hops)` clamps `hops` to `[1, MAX_GRAPH_HOPS=5]`
//! (D-14 DoS defense). Empty `entity_ids` short-circuits to an empty
//! result without touching storage. The Cypher template is portable across
//! Moon GRAPH.QUERY and Postgres AGE per D-16; W-7 alignment uses
//! `id_hex` as the match property.

use std::sync::Arc;

use lunaris_core::storage::types::Filter;
use lunaris_core::{Embedder, Hlc, KeywordPort, LunarisError, StoragePort};

// Plan 03-02: Re-export Graph constants alongside the builder so callers
// constructing a `recall()` chain can reach the hop / k defaults via the
// same `lunaris_retrieve::*` import surface.
pub use crate::operators::graph::{DEFAULT_GRAPH_HOPS, DEFAULT_GRAPH_K, Graph, MAX_GRAPH_HOPS};

use crate::hydrate::hydrate;
use crate::operators::modifiers::FilterParseError;
use crate::operators::vector::Vector;
use crate::operators::{QueryContext, Retriever};
use crate::types::{Hit, Query};

/// Builder returned by `Lunaris::recall()`.
pub struct RetrievalBuilder {
    pub(crate) root: Box<dyn Retriever>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) storage: Arc<dyn StoragePort>,
    pub(crate) keyword: Arc<dyn KeywordPort>,
    pub(crate) moon_storage: Option<Arc<lunaris_storage_moon::MoonStorage>>,
    pub(crate) base_filter: Option<Filter>,
    pub(crate) base_as_of: Option<Hlc>,
    /// Plan 04-04 B-9 fix: when `true`, hydration ORs this into every
    /// `Hit::degraded`. Set by `Lunaris::recall_with_degraded_check` when the
    /// verifier queue depth crosses `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD`.
    pub(crate) initial_degraded: bool,
}

impl RetrievalBuilder {
    /// Construct a fresh builder. Default root = `Vector::new("chunks", 30)`.
    pub fn new(
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        Self {
            root: Box::new(Vector::new("chunks", 30)),
            storage,
            keyword,
            embedder,
            moon_storage: None,
            base_filter: None,
            base_as_of: None,
            initial_degraded: false,
        }
    }

    /// Convenience constructor used by `Lunaris::recall()`. Same as `new`
    /// but takes the order callers are likely to think of (storage, keyword,
    /// embedder).
    pub fn from_handle(
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        Self::new(storage, keyword, embedder)
    }

    /// Wire a typed `MoonStorage` Arc into the builder so the Phase 1.5
    /// `fuse_rrf` Moon-native dispatch path can fire. The umbrella
    /// `Lunaris::recall()` calls this when the handle was opened against
    /// a `moon://` URL — caller code never needs to invoke this directly.
    pub fn with_moon_storage(mut self, moon: Arc<lunaris_storage_moon::MoonStorage>) -> Self {
        self.moon_storage = Some(moon);
        self
    }

    /// Replace the root retriever.
    ///
    /// `with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))`.
    pub fn with_root<R: Retriever + 'static>(mut self, root: R) -> Self {
        self.root = Box::new(root);
        self
    }

    /// Use a pre-built `Box<dyn Retriever>` as the root.
    pub fn with_root_boxed(mut self, root: Box<dyn Retriever>) -> Self {
        self.root = root;
        self
    }

    /// Set the structured filter that will narrow every operator's candidate
    /// set. Overrides any previous filter.
    pub fn filter(mut self, f: Filter) -> Self {
        self.base_filter = Some(f);
        self
    }

    /// Parse a v0 string DSL into a [`Filter`] and set it. Returns the parser
    /// error at builder time so callers see invalid syntax before any IO.
    pub fn filter_str(mut self, s: &str) -> Result<Self, FilterParseError> {
        let f = crate::operators::modifiers::filter_str(s)?;
        self.base_filter = Some(f);
        Ok(self)
    }

    /// Set the bi-temporal `as_of` snapshot timestamp.
    pub fn as_of(mut self, ts: Hlc) -> Self {
        self.base_as_of = Some(ts);
        self
    }

    /// Plan 04-04 B-9 fix: caller-set degraded flag plumbed through hydration.
    /// `Lunaris::recall_with_degraded_check` sets this when the verifier
    /// queue depth crosses `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` so every
    /// returned `Hit::degraded` is `true` (VERIFY-06 backpressure surface).
    ///
    /// Returns `self` so callers can chain into the rest of the builder.
    pub fn with_initial_degraded(mut self, deg: bool) -> Self {
        self.initial_degraded = deg;
        self
    }

    /// Wrap an upstream operator with `top(n)` at builder time. This is a
    /// convenience over `.with_root(prev_root.top(n))` — you can also call
    /// `.top(n)` directly on the operator before passing to `with_root`.
    pub fn top(self, n: usize) -> Self {
        let new_root = crate::operators::modifiers::TopRetriever::new(self.root, n);
        Self { root: Box::new(new_root), ..self }
    }

    /// Plan 02-03: Wrap the current root with a [`crate::RerankRetriever`].
    /// The cross-encoder reorders the top-30 (default) candidates and tags
    /// every hit with `rerank_applied = reranker.applies()` so callers can
    /// see whether the real model fired (vs the noop fallback).
    pub fn rerank(self, reranker: Arc<dyn lunaris_rerank::Reranker>) -> Self {
        let new_root = crate::operators::rerank::RerankRetriever::new(self.root, reranker);
        Self { root: Box::new(new_root), ..self }
    }

    /// Plan 02-03: Wrap the current root with a [`crate::DegradedFallbackRetriever`].
    /// On any error from the current (primary) root, the operator switches
    /// to `fallback` and tags returned hits with `degraded: true`.
    pub fn degraded_fallback<R: Retriever + 'static>(self, fallback: R) -> Self {
        let new_root = crate::operators::degraded::DegradedFallbackRetriever::new(
            self.root,
            Box::new(fallback),
        );
        Self { root: Box::new(new_root), ..self }
    }

    /// Run the tree and hydrate. Terminal — consumes the builder.
    pub async fn execute(self, mut query: Query) -> Result<Vec<Hit>, LunarisError> {
        // Builder-level filter / as_of override the per-query fields IFF the
        // query didn't already set them. Builder fields are the persistent
        // "this handle's defaults"; per-query fields are the one-shot
        // override.
        if query.filter.is_none() {
            query.filter = self.base_filter.clone();
        }
        if query.as_of.is_none() {
            query.as_of = self.base_as_of;
        }
        let as_of = query.as_of;
        // Plan 04-04 B-9: snapshot the initial_degraded flag BEFORE moving
        // the rest of the builder into the QueryContext / hydrate calls.
        let initial_degraded = self.initial_degraded;
        let ctx = match self.moon_storage.clone() {
            Some(moon) => QueryContext::with_moon(
                query,
                self.embedder,
                self.storage.clone(),
                self.keyword,
                moon,
            ),
            None => QueryContext::new(query, self.embedder, self.storage.clone(), self.keyword),
        };
        let raw = self.root.retrieve(&ctx).await?;
        hydrate(self.storage.as_ref(), raw, as_of, initial_degraded).await
    }
}
