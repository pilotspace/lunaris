//! `Keyword::bm25(index, k)` — runs `KeywordPort::keyword_search`.
//!
//! Both backends (`MoonStorage`, `PostgresStorage`) impl `KeywordPort` after
//! Plan 02-02 Task 1; this operator is backend-agnostic — the
//! [`super::QueryContext`] holds the right `Arc<dyn KeywordPort>` for the
//! handle that built it.

use std::any::Any;

use async_trait::async_trait;
use lunaris_core::LunarisError;

use super::{QueryContext, Retriever, clamp_k};
use crate::types::{RawHit, SourceOp};

/// Keyword (BM25) retrieval operator.
#[derive(Clone, Debug)]
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
}

#[async_trait]
impl Retriever for Keyword {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let hits = ctx
            .keyword
            .keyword_search(
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
                metadata: h.metadata,
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
