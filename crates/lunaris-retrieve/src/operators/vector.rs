//! `Vector::new(index, k)` — embed query, run `StoragePort::vector_search`.
//!
//! Per the [`super::QueryContext`] convention, this operator calls
//! [`super::QueryContext::embed_once`] which is cached per call so chained
//! `.and(Vector::new(..), other)` operators don't re-embed the query.

use std::any::Any;

use async_trait::async_trait;
use lunaris_core::LunarisError;

use super::{QueryContext, Retriever, clamp_k};
use crate::types::{RawHit, SourceOp};

/// Vector retrieval operator.
///
/// Construction is fluent — you wire it up at `recall()`-build time, the
/// network call only happens at `.execute()` time.
#[derive(Clone, Debug)]
#[must_use = "Vector is a query node — pass it to RetrievalBuilder::with_root() or chain via .and/.or/.fuse_rrf, otherwise it never executes"]
pub struct Vector {
    pub index: String,
    pub k: usize,
}

impl Vector {
    /// Build a new `Vector` operator targeting the given backend index
    /// (e.g., `"chunks"`) with `k` candidates.
    ///
    /// `k` is clamped to [`super::MAX_K`] (T-02-02-03 DoS mitigation).
    pub fn new(index: impl Into<String>, k: usize) -> Self {
        Self { index: index.into(), k: clamp_k(k) }
    }

    /// Convenience: wrap into the standard combinator builder. Implemented in
    /// `combinators` to avoid trait-method orphan rules with the generic
    /// `.and / .or / .then`.
    pub fn and<R: Retriever + 'static>(self, other: R) -> super::combinators::AndRetriever {
        super::combinators::AndRetriever::new(Box::new(self), Box::new(other))
    }
    pub fn or<R: Retriever + 'static>(self, other: R) -> super::combinators::OrRetriever {
        super::combinators::OrRetriever::new(Box::new(self), Box::new(other))
    }
    pub fn then<R: Retriever + 'static>(self, other: R) -> super::combinators::ThenRetriever {
        super::combinators::ThenRetriever::new(Box::new(self), Box::new(other))
    }
    /// Cap the final result set after the operator tree resolves.
    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }

    /// Wrap with a cross-encoder rerank pass (Plan 02-03).
    pub fn rerank(
        self,
        reranker: std::sync::Arc<dyn lunaris_rerank::Reranker>,
    ) -> super::rerank::RerankRetriever {
        super::rerank::RerankRetriever::new(Box::new(self), reranker)
    }

    /// Wrap with a fallback retriever — if THIS vector path errors, switch to
    /// `fallback` and tag returned hits with `degraded: true` (Plan 02-03).
    pub fn degraded_fallback<R: Retriever + 'static>(
        self,
        fallback: R,
    ) -> super::degraded::DegradedFallbackRetriever {
        super::degraded::DegradedFallbackRetriever::new(Box::new(self), Box::new(fallback))
    }
}

#[async_trait]
impl Retriever for Vector {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let q_emb = ctx.embed_once().await?;
        // Wave 2.5C: use ctx.scope — plumbed from RetrievalBuilder::with_scope
        // (set by ScopedLunaris::recall/dsl) so vector_search is scope-isolated
        // at the storage layer. Bare Lunaris::recall() uses Scope::dev().
        let hits = ctx
            .storage
            .vector_search(
                &ctx.scope,
                &self.index,
                &q_emb,
                self.k,
                ctx.query.filter.as_ref(),
                ctx.query.as_of,
                false,
            )
            .await?;
        Ok(hits
            .into_iter()
            .map(|h| RawHit {
                id: h.id,
                score: h.score,
                rerank_applied: h.rerank_applied,
                degraded: false,
                metadata: h.metadata,
                source_op: SourceOp::Vector,
            })
            .collect())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k_is_clamped() {
        let v = Vector::new("chunks", 1_000_000);
        assert_eq!(v.k, super::super::MAX_K);
    }

    #[test]
    fn vector_new_records_index() {
        let v = Vector::new("chunks", 30);
        assert_eq!(v.index, "chunks");
        assert_eq!(v.k, 30);
    }
}
