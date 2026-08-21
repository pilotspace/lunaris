//! `Navigate::new(index, k)` — graph-expanded vector retrieval (ADD task
//! `ft-navigate-recall`, contract v1).
//!
//! On backends with `capabilities().graph_navigate_native` (Moon), the
//! operator runs `StoragePort::vector_navigate` — KNN seeds, server-side BFS
//! over the scope graph, hop-aware re-rank — and each graph-expanded hit
//! carries `hop_depth` / `final_score` / `vec_score` provenance in its
//! metadata. On every other backend it degrades to plain `vector_search`
//! with byte-identical hit shape (recall never errors because navigation is
//! unavailable — same philosophy as `degraded_fallback`).

use std::any::Any;

use async_trait::async_trait;
use lunaris_core::storage::types::{GraphDecay, NavigateSpec};
use lunaris_core::{LunarisError, StorageError};
use serde_json::json;

use super::{QueryContext, Retriever, clamp_k};
use crate::types::{RawHit, SourceOp};

/// Default hop depth (matches `Graph::anchored`'s `DEFAULT_GRAPH_HOPS`).
pub const DEFAULT_NAVIGATE_HOPS: u32 = 2;

/// Graph-expanded vector retrieval operator.
///
/// Construction is fluent — wire it at `recall()`-build time, the network
/// call happens at `.retrieve()` time. Parameter validation happens at
/// construction (`with_hops` / `with_hop_penalty` return `Err` on invalid
/// values — nothing invalid ever reaches the wire).
#[derive(Clone, Debug)]
#[must_use = "Navigate is a query node — pass it to RetrievalBuilder::with_root() or chain via .and/.or/.fuse_rrf, otherwise it never executes"]
pub struct Navigate {
    pub index: String,
    pub k: usize,
    spec: NavigateSpec,
    /// Index the degraded vector path searches instead of `index`.
    ///
    /// `index` is the KNN SEED index; the leg's payload can live in a
    /// different one (KG-RAG facts-as-graph-nodes seeds on `entities` and
    /// hops to `facts`). Only Moon can hop, so on every other backend — and
    /// on any filtered query, which has no native navigate surface — the
    /// degraded search must target the payload index or the leg returns seed
    /// documents and its real content silently disappears. `None` keeps the
    /// original behaviour: degrade against the seed index.
    fallback_index: Option<String>,
}

impl Navigate {
    /// Build a `Navigate` operator targeting `index` (e.g. `"entities"` —
    /// the only production index whose docs are graph nodes) with `k`
    /// candidates and the default hop depth of 2.
    ///
    /// `k` is clamped to [`super::MAX_K`] (T-02-02-03 DoS mitigation).
    pub fn new(index: impl Into<String>, k: usize) -> Self {
        Self {
            index: index.into(),
            k: clamp_k(k),
            spec: NavigateSpec::new(DEFAULT_NAVIGATE_HOPS)
                .expect("DEFAULT_NAVIGATE_HOPS is within 1..=MAX_HOPS"),
            fallback_index: None,
        }
    }

    /// Point the degraded vector path at `index` instead of the KNN seed
    /// index. Use it whenever the seed and payload indexes differ — a facts
    /// leg seeding on `entities` MUST set `"facts"` here, or backends without
    /// native navigate return entity documents in place of facts.
    pub fn with_fallback_index(self, index: impl Into<String>) -> Self {
        Self { fallback_index: Some(index.into()), ..self }
    }

    /// The configured degraded-path index, if any.
    pub fn fallback_index(&self) -> Option<&str> {
        self.fallback_index.as_deref()
    }

    /// Set the hop depth (`1..=5`, Moon's `MAX_EXPAND_DEPTH`). Errors with
    /// `navigate_invalid_hops` before any IO.
    pub fn with_hops(self, hops: u32) -> Result<Self, LunarisError> {
        let mut spec = NavigateSpec::new(hops).map_err(LunarisError::Storage)?;
        if let Some(p) = self.spec.hop_penalty() {
            spec = spec.with_hop_penalty(p).map_err(LunarisError::Storage)?;
        }
        if let Some(d) = self.spec.decay() {
            spec = spec.with_decay(d);
        }
        Ok(Self { spec, ..self })
    }

    /// Set the per-hop score penalty (server default 0.1 when unset).
    /// Errors with `navigate_invalid_hop_penalty` before any IO.
    pub fn with_hop_penalty(self, p: f64) -> Result<Self, LunarisError> {
        let spec = self.spec.with_hop_penalty(p).map_err(LunarisError::Storage)?;
        Ok(Self { spec, ..self })
    }

    /// Apply recency decay to the discovery edge of each graph-expanded hit.
    /// Infallible — `GraphDecay` is validated by construction.
    pub fn with_decay(self, decay: GraphDecay) -> Self {
        Self { spec: self.spec.with_decay(decay), ..self }
    }

    /// The validated navigate parameters.
    pub fn spec(&self) -> &NavigateSpec {
        &self.spec
    }

    /// Convenience combinators — mirror `Vector`'s fluent surface.
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
    /// Wrap with a fallback retriever (Plan 02-03 semantics).
    pub fn degraded_fallback<R: Retriever + 'static>(
        self,
        fallback: R,
    ) -> super::degraded::DegradedFallbackRetriever {
        super::degraded::DegradedFallbackRetriever::new(Box::new(self), Box::new(fallback))
    }

    /// Plain vector-search fallback shared by the capability-gated and
    /// NotSupported degradation paths.
    async fn fallback_vector(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let index = self.fallback_index.as_deref().unwrap_or(&self.index);
        let q_emb = ctx.embed_once().await?;
        // F1: same rule as the leg above — an absent index means no rows.
        let hits = crate::missing_index::no_rows_if_index_absent(
            ctx.storage
                .vector_search(
                    &ctx.scope,
                    index,
                    &q_emb,
                    self.k,
                    ctx.query.filter.as_ref(),
                    ctx.query.as_of,
                    false,
                )
                .await,
        )?;
        Ok(hits
            .into_iter()
            .map(|h| RawHit {
                id: h.id,
                score: h.score,
                rerank_applied: h.rerank_applied,
                degraded: false,
                // Tag with the index actually searched — per-leg RRF bucketing
                // and the chunk-floor selector both route on this tag.
                metadata: super::tag_leg_index(h.metadata, index),
                source_op: SourceOp::Vector,
            })
            .collect())
    }
}

#[async_trait]
impl Retriever for Navigate {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        // ft-navigate-filter-gap contract v1 — Moon's FT.NAVIGATE has no
        // filter surface, so a filtered query MUST NOT take the native path:
        // it would silently ignore the filter and leak foreign hits via KNN
        // seeds and BFS expansion alike. Degrade to the filter-enforcing
        // vector path (Moon renders the filter server-side into the KNN
        // query). The guard sits ABOVE the capability gate: no backend has a
        // filtered navigate surface today. Trade-off: filtered recalls lose
        // graph expansion until the Moon-side FT.NAVIGATE FILTER follow-on.
        if ctx.query.filter.is_some() {
            tracing::debug!(
                index = %self.index,
                "navigate: filter present — degraded to filtered vector_search (no graph expansion)"
            );
            return self.fallback_vector(ctx).await;
        }
        if !ctx.storage.capabilities().graph_navigate_native {
            return self.fallback_vector(ctx).await;
        }
        let q_emb = ctx.embed_once().await?;
        let result =
            ctx.storage.vector_navigate(&ctx.scope, &self.index, &q_emb, self.k, &self.spec).await;
        let hits = match result {
            Ok(hits) => hits,
            // Defensive: a backend that reports the capability but still
            // refuses navigate degrades instead of failing the recall.
            Err(StorageError::NotSupported(_)) => return self.fallback_vector(ctx).await,
            // F1: a scope with no writes has no index to navigate. Empty, not
            // an error — and not a fallback either, since the fallback would
            // search the same absent index.
            Err(ref e) if crate::missing_index::is_index_absent(e) => return Ok(vec![]),
            Err(e) => return Err(LunarisError::Storage(e)),
        };
        Ok(hits
            .into_iter()
            // Hop-0 hits are the KNN SEEDS on the seed index (entity nodes
            // with no KV row): hydration drops them anyway, but only AFTER
            // `.top(k)` — so every seed that reaches fusion burns a fused/
            // rerank/final reader-context slot and then vanishes. The leg's
            // payload is the graph-EXPANDED content (hop >= 1); seeds exist
            // to anchor the hop, not to be results.
            .filter(|h| h.hop_depth >= 1)
            .map(|h| RawHit {
                id: h.id,
                // Moon's final_score is L2-style (lower = better); map into
                // the higher-is-better RawHit convention so downstream
                // `top()` / fusion ordering stays correct.
                score: 1.0 / (1.0 + h.final_score.max(0.0)),
                rerank_applied: false,
                degraded: false,
                // The "index" tag gives this leg its own RRF bucket (see
                // fuse.rs per-leg bucketing) — hop-penalty scores are
                // incomparable with genuine chunk cosines.
                metadata: json!({
                    "index": self.index,
                    "hop_depth": h.hop_depth,
                    "vec_score": h.vec_score,
                    "final_score": h.final_score,
                }),
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
        let n = Navigate::new("entities", 1_000_000);
        assert_eq!(n.k, super::super::MAX_K);
    }

    #[test]
    fn default_hops_is_two() {
        let n = Navigate::new("entities", 30);
        assert_eq!(n.spec().hops(), DEFAULT_NAVIGATE_HOPS);
    }

    #[test]
    fn with_hops_preserves_penalty_and_decay() {
        let n = Navigate::new("entities", 30)
            .with_hop_penalty(0.2)
            .unwrap()
            .with_decay(GraphDecay::new(1.5).unwrap())
            .with_hops(4)
            .unwrap();
        assert_eq!(n.spec().hops(), 4);
        assert_eq!(n.spec().hop_penalty(), Some(0.2));
        assert_eq!(n.spec().decay().map(|d| d.lambda()), Some(1.5));
    }
}
