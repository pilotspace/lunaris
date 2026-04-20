//! `StorageCapabilities` — what the backend supports natively.
//!
//! Higher layers (retrievers, recipes, the conformance suite) read this struct to
//! decide whether to degrade gracefully (e.g., skip rerank when `rerank_native=false`)
//! or refuse a query path (e.g., reject Cypher when `graph_native=false`).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCapabilities {
    /// `true` for Moon (`TEMPORAL.SNAPSHOT_AT`); emulated via columns on Postgres.
    pub bi_temporal_native: bool,
    /// `true` for Moon (CSR + Cypher) and Postgres (AGE extension).
    pub graph_native: bool,
    /// `true` for Moon's bundled cross-encoder; `false` for Postgres.
    pub rerank_native: bool,
    /// `true` for Moon (`MQ.*`) and Postgres (`pgmq`).
    pub queue_native: bool,
    /// Maximum vector dimension the backend's index can hold. Typically 768 (Moon
    /// EmbeddingGemma) or 1536 (Postgres+pgvector default upper bound).
    pub max_vector_dim: u32,
    /// `true` when the backend can run RRF (Reciprocal Rank Fusion) over
    /// `(vector, sparse_bm25)` natively in a single round trip — i.e., Moon's
    /// `FT.SEARCH ... HYBRID VECTOR ... SPARSE ... FUSION RRF WEIGHTS ...` exposed
    /// via `client.text().hybrid_search()`. Phase 2's `fuse_rrf` operator opts into
    /// `RrfFusion::Moon` when both branches hit a backend with `native_rrf=true` AND
    /// both branches are `Vector` / `Keyword(BM25)` operators on the same Moon index.
    /// Backends with `native_rrf=false` (e.g., Postgres) fall back to client-side
    /// fusion (`RrfFusion::Client`).
    pub native_rrf: bool,
}
