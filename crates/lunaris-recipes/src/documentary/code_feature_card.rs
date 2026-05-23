//! W2-L1 -- `CodeFeatureCard` multi-collection weighted-RRF recipe.
//!
//! ## Why per-collection weights?
//!
//! Helios stores code in three Lunaris collections:
//! - `"chunks"` -- raw source-file text (highest semantic density for code
//!   questions; a chunk IS the function body the agent asked about).
//! - `"symbols"` -- structured symbol index (function names, signatures, type
//!   definitions; secondary signal).
//! - `"commits"` -- git commit messages (prose context for history questions).
//!
//! Equal-weight RRF would rank a commit-message hit equal to a real code hit,
//! degrading recall quality for UC-I1. This recipe applies per-collection
//! trust multipliers before summing reciprocal-rank contributions.
//!
//! ## Profiles
//!
//! - [`QueryProfile::CodeQuestion`] -- default weights `chunks: 1.0, symbols:
//!   0.8, commits: 0.3`.
//! - [`QueryProfile::HistoryQuestion`] -- default weights `commits: 1.0,
//!   chunks: 0.5, symbols: 0.2`. Inverts priority for history queries.
//!
//! ## Graceful degradation
//!
//! The `symbols` and `commits` FT indexes are created by the W1-L1 schema
//! gate. Until that gate ships, both collections return empty on any Lunaris
//! instance that only has the base `chunks` index. `CodeFeatureCard::recall`
//! maps errors from the symbols / commits branches to empty lists rather than
//! failing the whole call.
//!
//! ## LOC contract
//!
//! File <= 700 LOC (CONTEXT.md sec 5). Public surface: `new`, `with_weights`,
//! `top`, `recall` -- four `pub fn` / `pub async fn` declarations.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{LunarisError, Scope, ScopeError};
use lunaris_retrieve::{Hit, Keyword, Query, Vector};
// SourceOp is only used in the test module's `make_hit` helper.
#[cfg(test)]
use lunaris_retrieve::SourceOp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// -- Public error type -------------------------------------------------------

/// Errors returned by [`CodeFeatureCard::recall`].
///
/// Library rule (CONTEXT.md sec 5): no `anyhow` in libs -- `thiserror` only.
#[derive(Debug, Error)]
pub enum RecallError {
    /// The underlying Lunaris storage returned an unexpected error on the
    /// `chunks` branch (the primary branch; symbol / commit branch errors
    /// degrade to empty rather than surfacing here).
    #[error("primary chunks branch failed: {0}")]
    ChunksBranchFailed(#[source] LunarisError),
    /// The caller supplied an invalid scope string (empty, too long, or
    /// invalid characters per `Scope::new`'s validation regex).
    #[error("invalid scope: {0}")]
    InvalidScope(#[from] ScopeError),
}

// -- Domain types ------------------------------------------------------------

/// Which query intent drives the collection weighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryProfile {
    /// "What does function X do?" -- semantic code questions.
    ///
    /// Default weights: `chunks 1.0 > symbols 0.8 > commits 0.3`.
    /// Rationale: a function-body chunk is the highest-quality answer;
    /// the symbol entry confirms name/type; commits describe *why* not *what*.
    CodeQuestion,

    /// "Why did this code change?" -- git-history questions.
    ///
    /// Default weights: `commits 1.0 > chunks 0.5 > symbols 0.2`.
    /// Rationale: the commit message is the primary signal for history intent.
    HistoryQuestion,
}

/// Per-collection trust multipliers for the weighted RRF formula.
///
/// For collection `i` with weight `w_i`, the contribution of a hit at rank
/// `r` (1-indexed) is `w_i / (k + r)` where `k` is the RRF constant (60).
/// Contributions are summed across all collections in which the item appears.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollectionWeights {
    /// Weight for the `"chunks"` collection (raw source-file text).
    pub chunks: f32,
    /// Weight for the `"symbols"` collection (function names, signatures).
    pub symbols: f32,
    /// Weight for the `"commits"` collection (git commit messages).
    pub commits: f32,
}

impl CollectionWeights {
    /// Defaults for [`QueryProfile::CodeQuestion`]:
    /// `chunks 1.0, symbols 0.8, commits 0.3`.
    ///
    /// Rationale: chunks are dense code text (highest semantic signal); symbols
    /// confirm type identity (secondary); commits describe *why* not *what*
    /// (lowest signal for code understanding).
    pub fn code_question() -> Self {
        Self { chunks: 1.0, symbols: 0.8, commits: 0.3 }
    }

    /// Defaults for [`QueryProfile::HistoryQuestion`]:
    /// `commits 1.0, chunks 0.5, symbols 0.2`.
    ///
    /// Rationale: the commit message is the primary artifact for history intent;
    /// the diff context (chunks) is secondary; symbol entries rarely appear in
    /// commit messages so they add minimal signal.
    pub fn history_question() -> Self {
        Self { commits: 1.0, chunks: 0.5, symbols: 0.2 }
    }
}

impl Default for CollectionWeights {
    /// Defaults to [`CollectionWeights::code_question`].
    fn default() -> Self {
        Self::code_question()
    }
}

// -- ScoredHit ---------------------------------------------------------------

/// A hydrated hit with its fused score and per-source provenance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoredHit {
    /// The hydrated [`Hit`] as returned by `lunaris-retrieve`.
    pub hit: Hit,
    /// Weighted RRF score: `sum(w_i / (k + rank_i))` across collections.
    pub score: f32,
    /// Which collection(s) this hit was found in, ordered by contribution
    /// (descending). Empty only when all weights are zero.
    pub provenance: Vec<HitProvenance>,
}

/// Records the collection that contributed a rank to this hit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitProvenance {
    /// Collection name: `"chunks"`, `"symbols"`, or `"commits"`.
    pub collection: String,
    /// 1-indexed rank within that collection (lower = higher score).
    pub rank: usize,
    /// Weighted contribution: `weight / (k + rank)`.
    pub contribution: f32,
}

// -- Recipe ------------------------------------------------------------------

/// Multi-collection recall recipe for code and history queries.
///
/// Fetches from three Lunaris collections (`chunks`, `symbols`, `commits`) in
/// parallel, then fuses results with a per-collection weighted RRF ranking.
///
/// ## Construction
///
/// ```no_run
/// use std::sync::Arc;
/// use lunaris::Lunaris;
/// use lunaris_recipes::documentary::code_feature_card::{CodeFeatureCard, QueryProfile};
///
/// # async fn demo(lunaris: Arc<Lunaris>) -> Result<(), Box<dyn std::error::Error>> {
/// let recipe = CodeFeatureCard::new(lunaris, QueryProfile::CodeQuestion);
/// let hits = recipe.recall("what does parse_token do", "default").await?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct CodeFeatureCard {
    lunaris: Arc<Lunaris>,
    /// Profile-driven default weights (overridable via [`Self::with_weights`]).
    pub profile: QueryProfile,
    /// Active per-collection weights.
    pub weights: CollectionWeights,
    /// Maximum number of hits returned after fusion.
    pub top_k: usize,
    /// RRF constant `k` in `1 / (k + rank)`. Cormack 2009 canonical = 60.
    rrf_k: usize,
}

/// Default `top_k` when not otherwise specified.
const DEFAULT_TOP_K: usize = 10;

/// Default RRF constant (Cormack 2009).
pub(crate) const DEFAULT_RRF_K: usize = 60;

/// Per-branch over-fetch multiplier.
const FANOUT: usize = 3;

impl CodeFeatureCard {
    /// Construct a new recipe with profile-driven default weights.
    pub fn new(lunaris: Arc<Lunaris>, profile: QueryProfile) -> Self {
        let weights = match profile {
            QueryProfile::CodeQuestion => CollectionWeights::code_question(),
            QueryProfile::HistoryQuestion => CollectionWeights::history_question(),
        };
        Self { lunaris, profile, weights, top_k: DEFAULT_TOP_K, rrf_k: DEFAULT_RRF_K }
    }

    /// Override the per-collection weights. Returns `self` for method-chaining.
    #[must_use = "with_weights returns the modified recipe; assign it back"]
    pub fn with_weights(mut self, weights: CollectionWeights) -> Self {
        self.weights = weights;
        self
    }

    /// Cap output at `k` hits. Returns `self` for method-chaining.
    #[must_use = "top returns the modified recipe; assign it back"]
    pub fn top(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    /// Execute the multi-collection recall against `scope_str` and return
    /// the top-k fused hits.
    ///
    /// Collections are fetched in parallel via `tokio::try_join!`. The
    /// `symbols` and `commits` branches map all errors to empty lists
    /// (graceful degradation -- W1-L1 schema gate may not be applied yet).
    pub async fn recall(
        &self,
        query: &str,
        scope_str: &str,
    ) -> Result<Vec<ScoredHit>, RecallError> {
        let k = self.top_k * FANOUT;
        let scope: Scope = Scope::new(scope_str)?;
        let q = query.to_owned();
        let rrf_k = self.rrf_k;

        // Parallel fan-out.
        let chunks_fut = {
            let lunaris = Arc::clone(&self.lunaris);
            let q = q.clone();
            let scope = scope.clone();
            async move { fetch_collection(&lunaris, &scope, "chunks", &q, k, rrf_k).await }
        };
        let symbols_fut = {
            let lunaris = Arc::clone(&self.lunaris);
            let q = q.clone();
            let scope = scope.clone();
            async move {
                fetch_collection_lenient(&lunaris, &scope, "symbols", &q, k, rrf_k).await
            }
        };
        let commits_fut = {
            let lunaris = Arc::clone(&self.lunaris);
            let q = q.clone();
            let scope = scope.clone();
            async move {
                fetch_collection_lenient(&lunaris, &scope, "commits", &q, k, rrf_k).await
            }
        };

        let (chunks_hits, symbols_hits, commits_hits) =
            tokio::try_join!(chunks_fut, symbols_fut, commits_fut)
                .map_err(RecallError::ChunksBranchFailed)?;

        let per_collection: Vec<(&str, Vec<Hit>, f32)> = vec![
            ("chunks", chunks_hits, self.weights.chunks),
            ("symbols", symbols_hits, self.weights.symbols),
            ("commits", commits_hits, self.weights.commits),
        ];
        let mut fused = fuse_weighted_per_collection(per_collection, self.rrf_k);
        fused.truncate(self.top_k);
        Ok(fused)
    }
}

// -- Internal helpers --------------------------------------------------------

/// Fetch `k` hits from `collection` via `Vector + Keyword RRF`.
async fn fetch_collection(
    lunaris: &Lunaris,
    scope: &Scope,
    collection: &str,
    query: &str,
    k: usize,
    rrf_k: usize,
) -> Result<Vec<Hit>, LunarisError> {
    let plan = Vector::new(collection, k)
        .and(Keyword::bm25(collection, k))
        .fuse_rrf(rrf_k as u32)
        .top(k);
    lunaris.recall().with_scope(scope.clone()).with_root(plan).execute(Query::text(query)).await
}

/// Fetch `k` hits from `collection`, mapping any error to empty.
async fn fetch_collection_lenient(
    lunaris: &Lunaris,
    scope: &Scope,
    collection: &str,
    query: &str,
    k: usize,
    rrf_k: usize,
) -> Result<Vec<Hit>, LunarisError> {
    match fetch_collection(lunaris, scope, collection, query, k, rrf_k).await {
        Ok(hits) => Ok(hits),
        Err(e) => {
            tracing::warn!(
                collection = collection,
                error = %e,
                "collection retrieval failed -- degrading to empty branch"
            );
            Ok(Vec::new())
        }
    }
}

/// Weighted RRF fusion across collections.
///
/// Groups hits by id (bytes), computes
/// `score = sum_i(weight_i / (k + rank_i))`
/// across all collections in which the item appears. Returns hits sorted
/// descending by fused score; ties broken by id ascending (deterministic).
///
/// `per_collection` -- `(collection_name, hits_sorted_by_score_desc, weight)`.
pub(crate) fn fuse_weighted_per_collection(
    per_collection: Vec<(&str, Vec<Hit>, f32)>,
    k: usize,
) -> Vec<ScoredHit> {
    use std::collections::HashMap;

    if per_collection.iter().all(|(_, hits, _)| hits.is_empty()) {
        return Vec::new();
    }

    let mut scores: HashMap<Vec<u8>, f32> = HashMap::new();
    let mut provenance_map: HashMap<Vec<u8>, Vec<HitProvenance>> = HashMap::new();
    let mut representative: HashMap<Vec<u8>, Hit> = HashMap::new();

    for (collection, hits, weight) in &per_collection {
        for (rank_0, hit) in hits.iter().enumerate() {
            let rank = rank_0 + 1; // 1-indexed
            let contribution =
                if *weight == 0.0 { 0.0 } else { weight / (k as f32 + rank as f32) };

            *scores.entry(hit.id.clone()).or_insert(0.0) += contribution;
            provenance_map.entry(hit.id.clone()).or_default().push(HitProvenance {
                collection: collection.to_string(),
                rank,
                contribution,
            });
            representative.entry(hit.id.clone()).or_insert_with(|| hit.clone());
        }
    }

    for prov in provenance_map.values_mut() {
        prov.sort_by(|a, b| {
            b.contribution.partial_cmp(&a.contribution).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let mut out: Vec<ScoredHit> = scores
        .into_iter()
        .filter_map(|(id, score)| {
            let hit = representative.remove(&id)?;
            let prov = provenance_map.remove(&id).unwrap_or_default();
            Some(ScoredHit { hit, score, provenance: prov })
        })
        .collect();

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hit.id.cmp(&b.hit.id))
    });

    out
}

// -- Unit tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::Hlc;

    fn make_hit(id: &[u8], score: f32, source: &str) -> Hit {
        Hit {
            id: id.to_vec(),
            score,
            text: format!("content for {source}"),
            source: source.to_string(),
            heading_path: Vec::new(),
            valid_from: Hlc::from_parts(1_000_000, 0, 0),
            valid_to: None,
            degraded: false,
            rerank_applied: false,
            source_op: SourceOp::Fused,
        }
    }

    // -- CollectionWeights defaults ------------------------------------------

    #[test]
    fn code_question_weights_rank_chunks_highest() {
        let w = CollectionWeights::code_question();
        assert!(w.chunks > w.symbols);
        assert!(w.symbols > w.commits);
    }

    #[test]
    fn history_question_weights_rank_commits_highest() {
        let w = CollectionWeights::history_question();
        assert!(w.commits > w.chunks);
        assert!(w.chunks > w.symbols);
    }

    #[test]
    fn default_weights_are_code_question() {
        assert_eq!(CollectionWeights::default(), CollectionWeights::code_question());
    }

    // -- fuse_weighted_per_collection ----------------------------------------

    #[test]
    fn code_question_ranks_chunk_above_commit() {
        let chunk_only = make_hit(b"chunk_a", 0.9, "chunk_a");
        let commit_only = make_hit(b"commit_b", 0.9, "commit_b");
        let per = vec![
            ("chunks", vec![chunk_only], 1.0_f32),
            ("symbols", vec![], 0.8_f32),
            ("commits", vec![commit_only], 0.3_f32),
        ];
        let fused = fuse_weighted_per_collection(per, 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].hit.id, b"chunk_a".to_vec());
        assert_eq!(fused[1].hit.id, b"commit_b".to_vec());
    }

    #[test]
    fn history_question_flips_ordering() {
        let chunk_only = make_hit(b"chunk_a", 0.9, "chunk_a");
        let commit_only = make_hit(b"commit_b", 0.9, "commit_b");
        let per = vec![
            ("chunks", vec![chunk_only], 0.5_f32),
            ("symbols", vec![], 0.2_f32),
            ("commits", vec![commit_only], 1.0_f32),
        ];
        let fused = fuse_weighted_per_collection(per, 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].hit.id, b"commit_b".to_vec());
        assert_eq!(fused[1].hit.id, b"chunk_a".to_vec());
    }

    #[test]
    fn weighted_rrf_is_deterministic() {
        let make_input = || {
            vec![
                ("chunks", vec![make_hit(b"a", 0.9, "a"), make_hit(b"b", 0.7, "b")], 1.0_f32),
                ("symbols", vec![make_hit(b"b", 0.8, "b"), make_hit(b"c", 0.6, "c")], 0.8_f32),
                ("commits", vec![make_hit(b"c", 0.85, "c"), make_hit(b"a", 0.5, "a")], 0.3_f32),
            ]
        };
        let run1 = fuse_weighted_per_collection(make_input(), 60);
        let run2 = fuse_weighted_per_collection(make_input(), 60);
        assert_eq!(run1.len(), run2.len());
        for (h1, h2) in run1.iter().zip(run2.iter()) {
            assert_eq!(h1.hit.id, h2.hit.id);
            assert!((h1.score - h2.score).abs() < 1e-7);
        }
    }

    #[test]
    fn empty_collection_does_not_panic() {
        let per: Vec<(&str, Vec<Hit>, f32)> = vec![
            ("chunks", vec![], 1.0),
            ("symbols", vec![], 0.8),
            ("commits", vec![], 0.3),
        ];
        assert!(fuse_weighted_per_collection(per, 60).is_empty());
    }

    #[test]
    fn zero_weight_collection_excluded_from_fusion() {
        let chunk_hit = make_hit(b"chunk_a", 0.9, "chunk_a");
        let commit_hit = make_hit(b"commit_z", 0.9, "commit_z");
        let per = vec![
            ("chunks", vec![chunk_hit], 1.0_f32),
            ("symbols", vec![], 0.8_f32),
            ("commits", vec![commit_hit], 0.0_f32),
        ];
        let fused = fuse_weighted_per_collection(per, 60);
        let commit_scored = fused.iter().find(|h| h.hit.id == b"commit_z".to_vec()).unwrap();
        let chunk_scored = fused.iter().find(|h| h.hit.id == b"chunk_a".to_vec()).unwrap();
        assert!(commit_scored.score.abs() < 1e-7, "zero-weight score must be 0");
        assert!(chunk_scored.score > commit_scored.score);
    }

    #[test]
    fn provenance_is_populated() {
        let chunk_hit = make_hit(b"a", 0.9, "chunk_a");
        let per = vec![
            ("chunks", vec![chunk_hit], 1.0_f32),
            ("symbols", vec![], 0.8_f32),
            ("commits", vec![], 0.3_f32),
        ];
        let fused = fuse_weighted_per_collection(per, 60);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].provenance.len(), 1);
        assert_eq!(fused[0].provenance[0].collection, "chunks");
        assert_eq!(fused[0].provenance[0].rank, 1);
    }

    #[test]
    fn multi_collection_hit_accumulates_score() {
        let a_chunk = make_hit(b"a", 0.9, "a_chunk");
        let a_commit = make_hit(b"a", 0.8, "a_commit"); // same id
        let b_chunk = make_hit(b"b", 0.7, "b");
        let per = vec![
            ("chunks", vec![a_chunk, b_chunk], 1.0_f32),
            ("symbols", vec![], 0.8_f32),
            ("commits", vec![a_commit], 0.3_f32),
        ];
        let fused = fuse_weighted_per_collection(per, 60);
        let a_hit = fused.iter().find(|h| h.hit.id == b"a".to_vec()).unwrap();
        let b_hit = fused.iter().find(|h| h.hit.id == b"b".to_vec()).unwrap();
        let expected_a = 1.0_f32 / 61.0 + 0.3 / 61.0;
        let expected_b = 1.0_f32 / 62.0;
        assert!((a_hit.score - expected_a).abs() < 1e-6);
        assert!((b_hit.score - expected_b).abs() < 1e-6);
        assert_eq!(fused[0].hit.id, b"a".to_vec());
    }

    #[test]
    fn tie_broken_by_id_ascending() {
        let hit_z = make_hit(b"z2", 0.9, "z2");
        let hit_a = make_hit(b"a2", 0.9, "a2");
        let per = vec![
            ("chunks", vec![hit_z], 1.0_f32),
            ("symbols", vec![hit_a], 1.0_f32),
            ("commits", vec![], 0.3_f32),
        ];
        let fused = fuse_weighted_per_collection(per, 60);
        assert_eq!(fused.len(), 2);
        assert!(fused[0].hit.id <= fused[1].hit.id);
    }

    // -- Parallel retrieve no waterfall --------------------------------------

    #[tokio::test]
    async fn parallel_retrieve_no_waterfall() {
        use std::time::{Duration, Instant};

        let delay = Duration::from_millis(100);
        let start = Instant::now();

        let f1 = async { tokio::time::sleep(delay).await; 1_usize };
        let f2 = async { tokio::time::sleep(delay).await; 2_usize };
        let f3 = async { tokio::time::sleep(delay).await; 3_usize };

        let (r1, r2, r3) = tokio::join!(f1, f2, f3);
        let elapsed = start.elapsed();

        assert_eq!(r1, 1);
        assert_eq!(r2, 2);
        assert_eq!(r3, 3);
        assert!(elapsed < Duration::from_millis(200), "must be parallel, took {elapsed:?}");
        assert!(elapsed >= Duration::from_millis(90), "sanity: not instant, took {elapsed:?}");
    }

    // -- QueryProfile --------------------------------------------------------

    #[test]
    fn query_profile_variants_are_distinct() {
        assert_ne!(
            std::mem::discriminant(&QueryProfile::CodeQuestion),
            std::mem::discriminant(&QueryProfile::HistoryQuestion),
        );
    }

    // -- LOC guard -----------------------------------------------------------

    #[test]
    fn code_feature_card_within_700_loc() {
        let src = include_str!("./code_feature_card.rs");
        let line_count = src.lines().count();
        assert!(line_count <= 700, "CONTEXT.md sec 5: file must be <=700 LOC; got {line_count}");
    }
}
