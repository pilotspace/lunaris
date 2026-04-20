//! `degraded_fallback(other)` — failure-resilience operator (RETRIEVE-08).
//!
//! Wraps a primary retriever with a fallback. When the primary returns
//! `Err(LunarisError)`, the operator transparently re-runs the same query
//! against the fallback retriever and tags every returned hit with
//! `RawHit { degraded: true }` so the caller can render "stale result" UI.
//!
//! ## Failure trigger
//!
//! ANY `LunarisError` from the primary triggers the flip — backend timeout,
//! connection drop, model OOM, partial-hydrate failure, anything. This is
//! the explicit RETRIEVE-08 contract per blueprint §8 ("if primary
//! disconnects, swap to fallback transparently").
//!
//! ## What about errors from the fallback?
//!
//! If the FALLBACK also errors, the operator surfaces the fallback's error
//! to the caller. The primary's error is logged via `tracing::warn` (with
//! `?error` so the full chain is captured for ops triage) but does NOT
//! short-circuit the fallback. This is the right semantic: callers wrap
//! `degraded_fallback` to defend against ONE backend failing; if BOTH fail
//! the recall is genuinely unavailable and the caller needs to know.
//!
//! ## Threat model (T-02-03-05)
//!
//! `degraded_fallback` MAY mask a security-relevant primary error (e.g., an
//! auth failure from Postgres). v0 accepts this — Phase 5 PROTO-05 (Bearer
//! auth) lands at the HTTP boundary BEFORE the retriever runs, so an auth
//! failure surfaces via `lunaris-server` middleware, not via the retriever.
//! Documented in the plan threat register.

use std::any::Any;

use async_trait::async_trait;
use lunaris_core::LunarisError;

use super::{QueryContext, Retriever};
use crate::types::RawHit;

/// `degraded_fallback(primary, fallback)` operator.
///
/// On any error from `primary`, runs `fallback` and tags returned hits with
/// `RawHit { degraded: true }`. Hydration copies that flag onto
/// `Hit { degraded: true }`.
pub struct DegradedFallbackRetriever {
    pub(crate) primary: Box<dyn Retriever>,
    pub(crate) fallback: Box<dyn Retriever>,
}

impl DegradedFallbackRetriever {
    pub fn new(primary: Box<dyn Retriever>, fallback: Box<dyn Retriever>) -> Self {
        Self { primary, fallback }
    }

    /// Convenience: cap the final result set after the fallback resolves.
    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }

    /// Wrap with a cross-encoder rerank pass (Plan 02-03). Lets callers chain
    /// `degraded_fallback(...).rerank(reranker).top(n)` when they want the
    /// rerank to apply REGARDLESS of which branch (primary vs fallback) ran.
    pub fn rerank(
        self,
        reranker: std::sync::Arc<dyn lunaris_rerank::Reranker>,
    ) -> super::rerank::RerankRetriever {
        super::rerank::RerankRetriever::new(Box::new(self), reranker)
    }
}

/// Builder-friendly factory: `degraded_fallback(primary, fallback)`.
pub fn degraded_fallback(
    primary: Box<dyn Retriever>,
    fallback: Box<dyn Retriever>,
) -> DegradedFallbackRetriever {
    DegradedFallbackRetriever::new(primary, fallback)
}

#[async_trait]
impl Retriever for DegradedFallbackRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        match self.primary.retrieve(ctx).await {
            Ok(hits) => Ok(hits),
            Err(e) => {
                // Per RETRIEVE-08: ANY primary error triggers the fallback.
                // Log the original error so ops can triage WHY the primary
                // failed even though the recall succeeded.
                tracing::warn!(
                    error = %e,
                    "primary retriever failed; falling back to secondary (Hit.degraded will be true)"
                );
                let mut fallback_hits = self.fallback.retrieve(ctx).await?;
                for h in &mut fallback_hits {
                    h.degraded = true;
                }
                Ok(fallback_hits)
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::StorageError;

    use crate::types::{RawHit, SourceOp};

    fn rh(id: &[u8], score: f32) -> RawHit {
        RawHit {
            id: id.to_vec(),
            score,
            rerank_applied: false,
            degraded: false,
            metadata: serde_json::Value::Null,
            source_op: SourceOp::Vector,
        }
    }

    /// Stub retriever — emits canned hits or a canned error.
    struct CannedRetriever {
        hits: Option<Vec<RawHit>>,
        err: Option<String>,
    }
    impl CannedRetriever {
        fn ok(hits: Vec<RawHit>) -> Self {
            Self { hits: Some(hits), err: None }
        }
    }
    #[async_trait]
    impl Retriever for CannedRetriever {
        async fn retrieve(&self, _ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
            if let Some(msg) = &self.err {
                return Err(LunarisError::Storage(StorageError::Backend(msg.clone())));
            }
            Ok(self.hits.clone().unwrap_or_default())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn fallback_wraps_two_retrievers() {
        let p = CannedRetriever::ok(vec![rh(b"a", 0.9)]);
        let f = CannedRetriever::ok(vec![rh(b"b", 0.5)]);
        let _r = degraded_fallback(Box::new(p), Box::new(f));
    }
}
