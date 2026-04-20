//! Core types — `Query`, `Hit`, `RawHit`, `SourceOp`.
//!
//! `Query` is what the user sends in; `Hit` is the hydrated, post-fusion
//! result they get back; `RawHit` is the pre-hydration shape that flows
//! between operators (carries the `SourceOp` tag so `fuse_rrf` can group
//! by branch and rank per group).

use lunaris_core::Hlc;
use lunaris_core::storage::types::Filter;
use serde::{Deserialize, Serialize};

/// What kind of operator produced a [`RawHit`]. RRF fusion groups raw hits by
/// this tag so per-branch rankings stay isolated when computing `1 / (k + rank_i)`.
///
/// Plan 02-03 added `Reranked` (cross-encoder pass output). Plan 03-02 added
/// `Graph` (anchored graph traversal output). The `Fused` variant marks a
/// `RawHit` that has already been through a `fuse_rrf` operator (so `fuse_rrf`
/// of a `fuse_rrf` re-fuses on the fused tag, treating it as a single branch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceOp {
    Vector,
    Keyword,
    Fused,
    Reranked,
    /// Plan 03-02 (D-15): `Graph::anchored` operator output. RRF fusion groups
    /// Graph hits as a separate branch from Vector/Keyword so reciprocal-rank
    /// contributions stay isolated. Per-hit score = `1.0 / (1 + bfs_rank)` for
    /// rank-stability across hops (rank 0 → 1.0, rank 1 → 0.5, …).
    Graph,
}

/// One pre-hydration retrieval hit. Operators flow these around between
/// each other; the final stage hydrates them into [`Hit`]s.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawHit {
    /// Backend-issued id (typically a ULID's 16 bytes).
    pub id: Vec<u8>,
    /// Score from the producing operator. Vector: cosine similarity.
    /// Keyword: min-max normalized BM25. Fused: sum of `1 / (k + rank_i)`
    /// across branches. Reranked: cross-encoder logit (replaces upstream
    /// score, see [`SourceOp::Reranked`]).
    pub score: f32,
    /// Whether a cross-encoder reranker has been applied to this hit.
    /// Plan 02-03's `rerank` operator sets this to the value of
    /// `Reranker::applies()` after the rerank pass — `true` when the real
    /// `BgeRerankerV2M3` ran, `false` when the `NoopReranker` passthrough
    /// fallback ran (degraded model-missing path per RETRIEVE-06).
    pub rerank_applied: bool,
    /// Set `true` by Plan 02-03's `degraded_fallback` operator when this
    /// hit came from the fallback (secondary) retriever after the primary
    /// errored. Default `false` — every upstream operator (Vector, Keyword,
    /// fuse_rrf, combinators, rerank) leaves it false; only the
    /// `degraded_fallback` operator flips it on the fallback path. Hydration
    /// copies this onto [`Hit::degraded`] so callers can tell whether the
    /// result came from the primary backend or the fallback.
    #[serde(default)]
    pub degraded: bool,
    /// Free-form metadata from the producing operator. Vector reads it from
    /// the backend's `__metadata` / payload column. Keyword reads it from
    /// the backend's payload column.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Which operator produced this hit. RRF fusion groups by this tag.
    pub source_op: SourceOp,
}

/// Final hydrated retrieval hit returned to the caller.
///
/// `text` + `source` come from the chunk's KV row (looked up via
/// `StoragePort::read_as_of` in [`crate::hydrate::hydrate`]). `valid_from` /
/// `valid_to` come from the chunk's bi-temporal stamp (so callers can render
/// "this fact was true on …").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub id: Vec<u8>,
    pub score: f32,
    /// Chunk text body — from the chunk's KV row.
    pub text: String,
    /// Episode source (e.g., `helios:fs/notes.md`). Empty when the episode
    /// row was not found at hydration time (e.g., since-deleted).
    pub source: String,
    /// Heading path inherited from the chunk. Empty list = root document.
    #[serde(default)]
    pub heading_path: Vec<String>,
    pub valid_from: Hlc,
    pub valid_to: Option<Hlc>,
    /// `true` when a `degraded_fallback` operator (Plan 02-03) flipped to a
    /// fallback retriever for this branch. Plumbed through hydration from
    /// [`RawHit::degraded`]. Callers can render "stale result" UI off this
    /// flag.
    pub degraded: bool,
    pub rerank_applied: bool,
    pub source_op: SourceOp,
}

/// One retrieval query.
///
/// Construct via [`Query::text`] for the common case (text-only with default
/// k=30 and no filter / as_of); use the struct-literal form when you need to
/// set every field explicitly.
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub text: String,
    pub k: usize,
    pub filter: Option<Filter>,
    pub as_of: Option<Hlc>,
}

impl Query {
    /// Build a default `Query` with the given text, `k = 30`, no filter, no `as_of`.
    pub fn text(t: impl Into<String>) -> Self {
        Self { text: t.into(), k: 30, filter: None, as_of: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_text_defaults() {
        let q = Query::text("hello");
        assert_eq!(q.text, "hello");
        assert_eq!(q.k, 30);
        assert!(q.filter.is_none());
        assert!(q.as_of.is_none());
    }

    #[test]
    fn source_op_is_hashable_and_eq() {
        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(SourceOp::Vector);
        s.insert(SourceOp::Vector);
        s.insert(SourceOp::Keyword);
        s.insert(SourceOp::Graph); // Plan 03-02 (D-15): Graph variant is its own bucket.
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn source_op_graph_is_distinct_from_other_variants() {
        // Plan 03-02 (D-15): the Graph variant must group separately from
        // Vector/Keyword/Fused/Reranked so RRF fusion can rank graph hits as
        // their own branch instead of folding them in with vector/keyword
        // results (which would skew the per-branch reciprocal-rank weights).
        assert_ne!(SourceOp::Graph, SourceOp::Vector);
        assert_ne!(SourceOp::Graph, SourceOp::Keyword);
        assert_ne!(SourceOp::Graph, SourceOp::Fused);
        assert_ne!(SourceOp::Graph, SourceOp::Reranked);
    }
}
