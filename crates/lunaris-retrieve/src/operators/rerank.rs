//! `rerank(model)` — cross-encoder rerank operator (RETRIEVE-06).
//!
//! Wraps an upstream retriever, takes its top-`k_in` (default 30) candidates,
//! partial-hydrates the chunk text for each, then calls the configured
//! [`Reranker`] to re-score and re-order them. Returns the reranked
//! `Vec<RawHit>` with `source_op = SourceOp::Reranked` and
//! `rerank_applied = reranker.applies()` so the downstream hydration step
//! can populate `Hit { rerank_applied }` truthfully.
//!
//! ## Why partial hydration HERE
//!
//! The cross-encoder needs the chunk body for pair encoding (`[CLS] query
//! [SEP] doc [SEP]`). `RawHit` is the pre-hydration shape, so this operator
//! calls [`crate::hydrate::partial_hydrate_text`] (chunk-only, NO Episode
//! lookup) before invoking the reranker. The full hydration that produces
//! `Hit` objects still runs at the end of the pipeline — it just hits the
//! same chunks twice in pathological cases. v0 accepts the duplicate read;
//! v1 may add a hot-cache.
//!
//! ## Hit-count contract
//!
//! [`Reranker::rerank`] MUST return exactly `docs.len()` items. The operator
//! validates the count after the call and returns
//! `LunarisError::Retrieve(RetrieveError::OperatorFailed)` on mismatch
//! (T-02-03-04 mitigation — guards against a buggy/spoofed reranker
//! dropping or duplicating docs).
//!
//! ## Latency budget
//!
//! Per blueprint §4.2 the rerank pass is allocated 12 ms p50 / 35 ms p99 on
//! CPU. The default `k_in = 30` matches the §4.2 spec; callers tune via
//! `rerank_with_top_in(upstream, reranker, k_in)`.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::{LunarisError, RetrieveError};
use lunaris_rerank::{RerankCandidate, Reranker};

use super::{QueryContext, Retriever};
use crate::hydrate::partial_hydrate_text;
use crate::types::{RawHit, SourceOp};

/// Default top-in for the rerank operator. Per blueprint §4.2 latency budget
/// the recall hot path passes top-30 candidates to the cross-encoder rerank
/// pass; downstream `.top(k)` truncates to the user-facing k (typically 5).
pub const DEFAULT_RERANK_TOP_IN: usize = 30;

/// `rerank(model)` operator — wraps an upstream retriever with a cross-encoder
/// rerank pass. See module-level rustdoc for the partial-hydration + hit-count
/// contracts.
pub struct RerankRetriever {
    pub(crate) upstream: Box<dyn Retriever>,
    pub(crate) reranker: Arc<dyn Reranker>,
    pub(crate) k_in: usize,
    /// W5 task 1 (abstention gate): absolute floor on the cross-encoder's
    /// sigmoid score (`bge-reranker-v2-m3` emits scores ∈ [0,1]). Hits
    /// scoring below the threshold are dropped — see the `retrieve()` impl
    /// for the exact staging relative to the count-validation contract.
    /// `None` (default) preserves the pre-W5 "reorder, never drop" behavior.
    pub(crate) min_score: Option<f32>,
}

impl RerankRetriever {
    /// Wrap `upstream` with a rerank pass using the default top-in
    /// ([`DEFAULT_RERANK_TOP_IN`] = 30).
    pub fn new(upstream: Box<dyn Retriever>, reranker: Arc<dyn Reranker>) -> Self {
        Self { upstream, reranker, k_in: DEFAULT_RERANK_TOP_IN, min_score: None }
    }

    /// Wrap with a caller-configured top-in. Use when the recall pipeline
    /// wants to send more or fewer candidates to the cross-encoder than the
    /// blueprint default.
    pub fn with_top_in(
        upstream: Box<dyn Retriever>,
        reranker: Arc<dyn Reranker>,
        k_in: usize,
    ) -> Self {
        Self { upstream, reranker, k_in, min_score: None }
    }

    /// W5 task 1: abstention gate. Cross-encoder hits scoring below
    /// `min_score` are dropped from the result set instead of being returned
    /// as "best of the worst" — an empty `Vec` is a legitimate abstention
    /// signal the caller can render as "no confident memory found" rather
    /// than grounding a wrong answer on a low-relevance hit.
    ///
    /// `min_score` is compared against the cross-encoder's raw sigmoid
    /// output ∈ [0,1] (`bge-reranker-v2-m3`). The gate is skipped entirely
    /// when the configured [`Reranker::applies`] is `false` (the
    /// `NoopReranker` degraded-fallback path) — see `retrieve()` for why.
    ///
    /// Non-breaking: default is `None`, which preserves the pre-W5 ordering
    /// contract exactly (every existing caller is unaffected).
    pub fn with_min_score(mut self, min_score: f32) -> Self {
        self.min_score = Some(min_score);
        self
    }

    /// Convenience: cap the final result set after the rerank pass resolves.
    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }

    /// Wrap with a fallback retriever — if THIS rerank path errors, switch to
    /// `fallback` and tag returned hits with `degraded: true` (Plan 02-03).
    pub fn degraded_fallback<R: Retriever + 'static>(
        self,
        fallback: R,
    ) -> super::degraded::DegradedFallbackRetriever {
        super::degraded::DegradedFallbackRetriever::new(Box::new(self), Box::new(fallback))
    }
}

/// Builder-friendly factory: `rerank(upstream, reranker)`. Mirrors the
/// `fuse_rrf(inner, k)` shape from `operators/fuse.rs` so callers can
/// chain via either method-style or function-style composition.
pub fn rerank(upstream: Box<dyn Retriever>, reranker: Arc<dyn Reranker>) -> RerankRetriever {
    RerankRetriever::new(upstream, reranker)
}

/// Builder-friendly factory: `rerank_with_threshold(upstream, reranker, k_in,
/// min_score)` — W5 task 1's DSL builder surface. Combines
/// [`RerankRetriever::with_top_in`] and [`RerankRetriever::with_min_score`]
/// in one call for the common "widen the pool AND abstain below threshold"
/// composition.
pub fn rerank_with_threshold(
    upstream: Box<dyn Retriever>,
    reranker: Arc<dyn Reranker>,
    k_in: usize,
    min_score: f32,
) -> RerankRetriever {
    RerankRetriever::with_top_in(upstream, reranker, k_in).with_min_score(min_score)
}

#[async_trait]
impl Retriever for RerankRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        // 1. Run upstream → Vec<RawHit>.
        let mut raw = self.upstream.retrieve(ctx).await?;

        // 2. Truncate to top k_in (default 30) BEFORE rerank — per blueprint §4.2,
        //    the pass is sized for top-30 → top-k. Sort by upstream score so the
        //    truncation keeps the most-promising candidates.
        raw.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        raw.truncate(self.k_in);

        if raw.is_empty() {
            return Ok(raw);
        }

        // 3. Partial-hydrate text for each surviving id (chunk-only, no Episode
        //    lookup — text is all the cross-encoder needs).
        // Wave 2.5C: pass ctx.scope so partial_hydrate_text reads from the
        // correct scope partition (not Scope::dev()).
        let texts =
            partial_hydrate_text(ctx.storage.as_ref(), &ctx.scope, &raw, ctx.query.as_of).await?;

        // 4. Build candidate list. Hits without a hydrated text get an empty
        //    string — the cross-encoder will produce a low score and the
        //    downstream `top(n)` will likely truncate them.
        let raw_for_zip = raw.clone();
        let candidates: Vec<RerankCandidate> = raw
            .into_iter()
            .map(|h| RerankCandidate {
                text: texts.get(&h.id).cloned().unwrap_or_default(),
                id: h.id,
                score: h.score,
                metadata: h.metadata,
            })
            .collect();

        let candidate_count = candidates.len();
        tracing::debug!(
            rerank_applied = self.reranker.applies(),
            candidate_count,
            "running rerank pass"
        );

        // 5. Invoke reranker.
        let reranked = self.reranker.rerank(&ctx.query.text, candidates).await?;

        // 6. Validate hit count (T-02-03-04 mitigation — guards against a
        //    spoofed/buggy reranker dropping or duplicating docs). This MUST
        //    run against the reranker's raw output, BEFORE the W5 abstention
        //    gate below — count validation is an anti-spoof invariant, the
        //    threshold drop is a separate, legitimate content decision. Do
        //    not fold them into one check.
        if reranked.len() != candidate_count {
            return Err(LunarisError::Retrieve(RetrieveError::OperatorFailed(format!(
                "reranker returned {} docs, expected {}",
                reranked.len(),
                candidate_count
            ))));
        }

        // 7. W5 task 1 — rerank abstention gate. Drop hits scoring below
        //    `min_score`, AFTER the count-validation contract above (a
        //    separate stage, per module docs).
        //
        //    Only applied when a REAL cross-encoder fired
        //    (`reranker.applies() == true`). `NoopReranker`'s passthrough
        //    score is the UPSTREAM operator's own scale (cosine similarity /
        //    normalized BM25 / RRF sum) — not the bge-reranker-v2-m3 sigmoid
        //    ∈ [0,1] the threshold is calibrated against. Applying the gate
        //    on that mismatched scale would silently turn "model weights
        //    missing" into "abstained on every query", which is a worse
        //    failure mode than the un-gated degraded passthrough. An empty
        //    `Vec` after gating is a legitimate abstention signal and flows
        //    through unchanged (no special-casing needed below).
        let applies = self.reranker.applies();
        let reranked: Vec<RerankCandidate> = match self.min_score {
            Some(threshold) if applies => {
                reranked.into_iter().filter(|c| c.score >= threshold).collect()
            }
            _ => reranked,
        };

        // 8. Map back to RawHit; preserve degraded flag from upstream (rerank
        //    does NOT introduce degradation; degraded_fallback does).
        let degraded_by_id: std::collections::HashMap<Vec<u8>, bool> =
            raw_for_zip.into_iter().map(|h| (h.id, h.degraded)).collect();
        Ok(reranked
            .into_iter()
            .map(|c| RawHit {
                degraded: degraded_by_id.get(&c.id).copied().unwrap_or(false),
                id: c.id,
                score: c.score,
                rerank_applied: applies,
                metadata: c.metadata,
                source_op: SourceOp::Reranked,
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
    use lunaris_rerank::NoopReranker;

    #[test]
    fn default_top_in_is_30() {
        // Per blueprint §4.2 latency budget — top-30 → top-k rerank pass.
        assert_eq!(DEFAULT_RERANK_TOP_IN, 30);
    }

    #[test]
    fn rerank_factory_builds_with_default_k_in() {
        // Construct an upstream stub that always returns empty.
        struct EmptyRetriever;
        #[async_trait]
        impl Retriever for EmptyRetriever {
            async fn retrieve(&self, _ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
                Ok(Vec::new())
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        let r = rerank(Box::new(EmptyRetriever), Arc::new(NoopReranker));
        assert_eq!(r.k_in, DEFAULT_RERANK_TOP_IN);
    }
}
