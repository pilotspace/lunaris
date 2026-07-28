//! `Keyword::bm25(index, k)` — runs `KeywordPort::keyword_search`.
//!
//! `MoonStorage` impls `KeywordPort` (0.7.0 is Moon-only; the `PostgresStorage`
//! impl went with its backend). This operator stays backend-agnostic — the
//! [`super::QueryContext`] holds the right `Arc<dyn KeywordPort>` for the
//! handle that built it.

use std::any::Any;

use async_trait::async_trait;
use lunaris_core::LunarisError;

use super::{QueryContext, Retriever, clamp_k};
use crate::types::{RawHit, SourceOp};

/// Keyword (BM25) retrieval operator.
#[derive(Clone, Debug)]
#[must_use = "Keyword is a query node — pass it to RetrievalBuilder::with_root() or chain via .and/.or/.fuse_rrf, otherwise it never executes"]
pub struct Keyword {
    pub index: String,
    pub k: usize,
}

impl Keyword {
    /// Build a new BM25 operator. `k` is clamped to [`super::MAX_K`].
    pub fn bm25(index: impl Into<String>, k: usize) -> Self {
        Self { index: index.into(), k: clamp_k(k) }
    }

    pub fn and<R: Retriever + 'static>(self, other: R) -> super::combinators::AndRetriever {
        super::combinators::AndRetriever::new(Box::new(self), Box::new(other))
    }
    pub fn or<R: Retriever + 'static>(self, other: R) -> super::combinators::OrRetriever {
        super::combinators::OrRetriever::new(Box::new(self), Box::new(other))
    }
    pub fn then<R: Retriever + 'static>(self, other: R) -> super::combinators::ThenRetriever {
        super::combinators::ThenRetriever::new(Box::new(self), Box::new(other))
    }
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

    /// Wrap with a fallback retriever — if THIS keyword path errors, switch to
    /// `fallback` and tag returned hits with `degraded: true` (Plan 02-03).
    pub fn degraded_fallback<R: Retriever + 'static>(
        self,
        fallback: R,
    ) -> super::degraded::DegradedFallbackRetriever {
        super::degraded::DegradedFallbackRetriever::new(Box::new(self), Box::new(fallback))
    }
}

#[async_trait]
impl Retriever for Keyword {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        // Wave 2.5A: thread scope into keyword_search (RFC 0001 §3.4 amendment).
        // Wave 2.5C plumbs the real per-call scope from ctx.scope; until then
        // ctx.scope == Scope::dev() for bare Lunaris::recall() callers and the
        // caller-supplied scope for ScopedLunaris::recall() callers.
        let hits = ctx
            .keyword
            .keyword_search(
                &ctx.scope,
                &self.index,
                &ctx.query.text,
                self.k,
                ctx.query.filter.as_ref(),
                ctx.query.as_of,
            )
            .await?;
        Ok(hits
            .into_iter()
            .map(|h| RawHit {
                id: h.id,
                score: h.score,
                rerank_applied: false,
                degraded: false,
                metadata: super::tag_leg_index(h.metadata, &self.index),
                source_op: SourceOp::Keyword,
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
    fn keyword_bm25_records_fields() {
        let k = Keyword::bm25("chunks", 30);
        assert_eq!(k.index, "chunks");
        assert_eq!(k.k, 30);
    }

    #[test]
    fn keyword_k_is_clamped() {
        let k = Keyword::bm25("chunks", 1_000_000);
        assert_eq!(k.k, super::super::MAX_K);
    }
}
