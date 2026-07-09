//! lunaris-retrieve — the v0 retrieval DSL.
//!
//! Per blueprint §8 the retrieval pipeline must "feel like Keras, not like a
//! query language" — declarative, composable, `tower::Service`-shaped at the
//! edge so middleware (rate-limit, retry, timeout, tracing) drops in for free.
//!
//! ## Quick example
//!
//! ```ignore
//! use lunaris_retrieve::{Vector, Keyword, Query, RetrievalBuilder};
//!
//! // The DSL composes (no .await needed at construction):
//! let plan = Vector::new("chunks", 30)
//!     .and(Keyword::bm25("chunks", 30))
//!     .fuse_rrf(60)
//!     .top(5);
//! ```
//!
//! The `Lunaris::recall()` umbrella API constructs a [`RetrievalBuilder`]
//! seeded with the handle's storage / embedder / keyword / clock and
//! a default root of `Vector::new("chunks", 30)`. Callers swap the root
//! via [`RetrievalBuilder::with_root`].
//!
//! ## Module layout
//!
//! - [`types`] — `Query`, `Hit`, `RawHit`, `SourceOp`, `Plan`
//! - [`operators`] — `Retriever` trait + `QueryContext` + the operator structs
//! - [`hydrate()`] — batched chunk-text + source hydration
//! - [`planner`] — `plan_query` heuristic stub (RETRIEVE-13)
//! - [`service`] — `RetrievalService` impl of `tower::Service<Query>`
//! - [`builder`] — `RetrievalBuilder` (terminal `.execute()` wires the tree
//!   to the `QueryContext` and runs hydrate)
//! - [`fusion`] — Moon-native vs client-side RRF dispatch (consumes the
//!   Phase 1.5 `RrfFusion` enum + `StorageCapabilities::native_rrf`)

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod builder;
pub mod fusion;
pub mod hydrate;
pub mod operators;
pub mod planner;
pub mod service;
pub mod types;

pub use builder::RetrievalBuilder;
pub use hydrate::{hydrate, partial_hydrate_text};
// W5 task 3: FT.AGGREGATE deterministic counting/grouping operator. NOT a
// `Retriever` (see module docs) — re-exported at the crate root alongside
// the other operator surfaces so recipes/MCP tools can
// `use lunaris_retrieve::{Aggregate, AggregateReducer, AggregateGroup}`.
pub use operators::aggregate::{Aggregate, AggregateGroup, AggregateReducer};
pub use operators::combinators::{AndRetriever, OrRetriever, ThenRetriever, then};
pub use operators::degraded::{DegradedFallbackRetriever, degraded_fallback};
pub use operators::fuse::FuseRrfRetriever;
pub use operators::graph::{
    DEFAULT_GRAPH_HOPS, DEFAULT_GRAPH_K, Graph, LUNARIS_GRAPH_NAME, MAX_GRAPH_HOPS,
};
pub use operators::keyword::Keyword;
pub use operators::modifiers::{TopRetriever, filter_str};
pub use operators::navigate::{DEFAULT_NAVIGATE_HOPS, Navigate};
pub use operators::recency::{
    ACT_R_MIN_AGE_SECONDS, ActR, Exp, RecencyConfig, RecencyScorer, TimeSource, rescore_recency,
};
pub use operators::rerank::{DEFAULT_RERANK_TOP_IN, RerankRetriever, rerank};
pub use operators::tree::Tree;
pub use operators::vector::Vector;
pub use operators::{QueryContext, Retriever};
pub use planner::{Plan, plan_query};
pub use service::RetrievalService;
pub use types::{Hit, Query, RawHit, SourceOp};

// Plan 02-03: Reranker trait + helpers re-exported from lunaris-rerank so
// callers `use lunaris_retrieve::{Reranker, NoopReranker}` instead of
// reaching into the lower crate.
pub use lunaris_rerank::{NoopReranker, RerankCandidate, Reranker};

// Plan 03-02: EntityId re-exported from lunaris-extract so callers `use
// lunaris_retrieve::EntityId` to construct `Graph::anchored(entity_ids, hops)`
// without reaching across to the extractor crate. EntityId is the only
// extractor-side DTO needed by the retrieve surface — backends + Validator
// stay behind their own re-exports in the umbrella `lunaris` crate.
pub use lunaris_extract::EntityId;
