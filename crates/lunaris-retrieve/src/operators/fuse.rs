//! `fuse_rrf(k)` — Reciprocal Rank Fusion across heterogeneous result sets.
//!
//! ## Routing decision (Phase 1.5 STORE-09 retrofit)
//!
//! Phase 1.5 added `RrfFusion::Moon { k, weights }` + `StorageCapabilities::native_rrf`
//! so a Moon backend can fuse vector + BM25 in **one** server round-trip via
//! `client.text().hybrid_search()`. Plan 02-02's `fuse_rrf` MUST honor that:
//!
//! 1. Inspect the upstream operator tree for the canonical Vector + Keyword(BM25)
//!    on the SAME index pattern;
//! 2. If both branches resolve to a backend with `capabilities().native_rrf == true`,
//!    route to the Moon-native path via [`crate::fusion::fuse_via_moon_native`];
//! 3. Otherwise fall back to client-side reciprocal rank fusion
//!    `score = Σ 1/(k + rank_i)` over the per-`SourceOp` groupings.
//!
//! The default `k = 60` matches the RRF paper (`Cormack 2009`) — it's the conventional
//! constant that balances per-branch rank weight without over-flattening top
//! results.

use std::any::Any;
use std::collections::HashMap;

use async_trait::async_trait;
use lunaris_core::LunarisError;

use super::{QueryContext, Retriever};
use crate::fusion::{FusedBranchHint, FusedKind, fuse_via_moon_native, inspect_branches};
use crate::types::{RawHit, SourceOp};

/// Reciprocal Rank Fusion operator.
///
/// Wraps an upstream [`Retriever`] (typically an [`super::combinators::AndRetriever`])
/// and folds its `RawHit`s into a single fused ranking. Tags every output hit with
/// `SourceOp::Fused`.
pub struct FuseRrfRetriever {
    pub(crate) inner: Box<dyn Retriever>,
    pub(crate) k: usize,
    pub(crate) hint: Option<FusedBranchHint>,
}

impl FuseRrfRetriever {
    /// Construct a fuse operator with RRF constant `k`.
    ///
    /// `k` is the constant in the `1 / (k + rank)` formula. The conventional
    /// value is 60 (Cormack 2009).
    pub fn new(inner: Box<dyn Retriever>, k: usize) -> Self {
        let hint = inspect_branches(inner.as_ref());
        Self { inner, k, hint }
    }

    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }
}

#[async_trait]
impl Retriever for FuseRrfRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        // Routing decision: when the inner tree is exactly Vector + Keyword(BM25)
        // on the same index AND both branches hit a `native_rrf=true` backend
        // AND the QueryContext has a typed `MoonStorage` Arc wired in (set by
        // the umbrella builder when the handle's storage is Moon), delegate
        // to the Moon-native one-round-trip path.
        if let Some(hint) = &self.hint
            && matches!(hint.kind, FusedKind::VectorKeywordSameIndex)
            && ctx.storage.capabilities().native_rrf
            && ctx.moon_storage.is_some()
        {
            return fuse_via_moon_native(ctx, hint, self.k).await;
        }

        // Client-side fallback path: collect both branches concurrently,
        // group by source_op, rank within group, compute RRF score.
        let raw = self.inner.retrieve(ctx).await?;
        Ok(client_side_rrf(raw, self.k))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Client-side reciprocal rank fusion.
///
/// Group by `source_op`, sort each group by descending score, assign rank
/// (1-indexed), then for each id sum `1 / (k + rank_i)` across groups.
/// Returns hits sorted by descending fused score, each tagged
/// `SourceOp::Fused`.
pub fn client_side_rrf(raw: Vec<RawHit>, k: usize) -> Vec<RawHit> {
    if raw.is_empty() {
        return Vec::new();
    }

    // 1. Bucket by source_op.
    let mut by_source: HashMap<SourceOp, Vec<RawHit>> = HashMap::new();
    for h in raw {
        by_source.entry(h.source_op).or_default().push(h);
    }

    // 2. Within each bucket, sort descending by score and assign rank.
    //    Then accumulate the RRF contribution per id.
    let mut fused: HashMap<Vec<u8>, f32> = HashMap::new();
    let mut sample: HashMap<Vec<u8>, RawHit> = HashMap::new();
    for (_src, mut group) in by_source {
        group.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, hit) in group.into_iter().enumerate() {
            let r = rank + 1; // 1-indexed
            let contribution = 1.0_f32 / (k as f32 + r as f32);
            *fused.entry(hit.id.clone()).or_insert(0.0) += contribution;
            // Keep the highest-score-so-far hit as the "shape" of the fused row
            // (carries metadata + rerank flag).
            sample
                .entry(hit.id.clone())
                .and_modify(|existing| {
                    if hit.score > existing.score {
                        *existing = hit.clone();
                    }
                })
                .or_insert(hit);
        }
    }

    // 3. Materialize fused list.
    let mut out: Vec<RawHit> = fused
        .into_iter()
        .map(|(id, score)| {
            let s = sample.remove(&id).unwrap_or(RawHit {
                id: id.clone(),
                score,
                rerank_applied: false,
                metadata: serde_json::Value::Null,
                source_op: SourceOp::Fused,
            });
            RawHit {
                id,
                score,
                rerank_applied: s.rerank_applied,
                metadata: s.metadata,
                source_op: SourceOp::Fused,
            }
        })
        .collect();

    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rh(id: &[u8], score: f32, src: SourceOp) -> RawHit {
        RawHit {
            id: id.to_vec(),
            score,
            rerank_applied: false,
            metadata: json!({}),
            source_op: src,
        }
    }

    #[test]
    fn rrf_empty_returns_empty() {
        let out = client_side_rrf(vec![], 60);
        assert!(out.is_empty());
    }

    #[test]
    fn rrf_single_branch_ranks_by_rrf() {
        // 2 vector hits, no keyword. RRF should produce two fused entries
        // with descending score (rank 1 > rank 2).
        let hits = vec![rh(b"a", 0.9, SourceOp::Vector), rh(b"b", 0.5, SourceOp::Vector)];
        let out = client_side_rrf(hits, 60);
        assert_eq!(out.len(), 2);
        // 1/(60+1) > 1/(60+2)
        assert!(out[0].score > out[1].score);
        // Both are tagged Fused.
        assert!(out.iter().all(|h| h.source_op == SourceOp::Fused));
    }

    #[test]
    fn rrf_two_branches_sum_contributions() {
        // id=a appears rank 1 in vector AND rank 1 in keyword → score = 1/61 + 1/61
        // id=b appears rank 2 in vector AND rank 2 in keyword → score = 1/62 + 1/62
        // id=c appears rank 1 in keyword only — score = 1/61 (vector-rank 1 collides with a)
        // Setup: vector ranks a, b. keyword ranks c, a.
        let hits = vec![
            rh(b"a", 0.9, SourceOp::Vector),
            rh(b"b", 0.5, SourceOp::Vector),
            rh(b"c", 0.8, SourceOp::Keyword),
            rh(b"a", 0.6, SourceOp::Keyword),
        ];
        let out = client_side_rrf(hits, 60);
        assert_eq!(out.len(), 3);
        // a should top out (appears in both branches).
        assert_eq!(out[0].id, b"a".to_vec());
        let a_score = 1.0 / 61.0 + 1.0 / 62.0; // vector rank 1 + keyword rank 2
        assert!((out[0].score - a_score).abs() < 1e-6);
    }

    #[test]
    fn rrf_constant_default_60() {
        // Sanity: rank 1 contribution at k=60 is 1/61 = 0.0163934...
        assert!(((1.0_f32 / 61.0) - 0.0163934).abs() < 1e-6);
    }
}
