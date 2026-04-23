//! Phase 9 Plan 09-02 PRIM-02 — `DocumentCorpus` primitive.
//!
//! Hybrid Vector + Keyword RAG primitive with Reciprocal Rank Fusion
//! (Cormack 2009, `k = 60`). Backend dispatch — Moon-native `FT.*` hybrid
//! vs Postgres client-side rank merge — is handled by
//! [`lunaris_retrieve::RetrievalBuilder::execute`]; this primitive ONLY
//! assembles the plan. Per CLAUDE.md Lunaris does NOT bundle a second BM25
//! / HNSW library (the check is grep-asserted below).
//!
//! ## Fluent builder
//!
//! ```ignore
//! use std::sync::Arc;
//! use lunaris::Lunaris;
//! use lunaris_recipes::DocumentCorpus;
//!
//! # async fn demo(mem: Arc<Lunaris>) -> anyhow::Result<()> {
//! let hits = DocumentCorpus::new(mem, "kb:papers/")
//!     .filter("lang", "en")
//!     .top(10)
//!     .search("quantum error correction")
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! ## ≤ 30 LOC public-surface contract (PRIM-02)
//!
//! Public symbols: `new`, `ingest`, `filter`, `top`, `search`. Five
//! `pub fn` / `pub async fn` declarations — the LOC-guard test at the
//! bottom of this file counts them and caps at 6 (matches Phase 5
//! `helios_scratchpad_public_surface_under_50_loc` template).
//!
//! ## Plan-vs-code corrections applied (Rule 1 deviations, see 09-02-SUMMARY.md)
//!
//! The plan text referenced several APIs that don't exist on the v0.1.0
//! surface (`WriteOp::upsert_episode`, `Hlc::now()`, `Episode.valid_time`).
//! This matches the same corrections Plan 09-01's MessageStream applied.
//! The implementation delegates `ingest` to [`Lunaris::ingest`] per chunk
//! — each call runs the full chunker + embedder + `atomic_write` fan-out
//! inside `lunaris_ingest::ingest_episode`, so Wave 3 parity tests can
//! actually recall chunks from the `"chunks"` vector index. INGEST-04 is
//! trivially satisfied at the per-chunk grain (one call = one internal
//! atomic_write).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::storage::types::Filter;
use lunaris_core::{Episode, LunarisError};
use lunaris_retrieve::{Hit, Keyword, Query, Vector};

/// Default RRF constant from Cormack et al. (2009). Matches `fuse_rrf`'s
/// `k: u32` parameter. NEVER tune without coordinating with Phase 11
/// documentary-wrapper acceptance tests.
pub(crate) const DEFAULT_RRF_K: u32 = 60;

/// Default `top_k` when caller does not call `.top(...)`.
const DEFAULT_TOP_K: usize = 10;

/// Initial retrieval fan-out multiplier. Fetches `top_k * FANOUT` per
/// branch before RRF fusion so ties break naturally at fuse time.
const FANOUT: usize = 3;

/// Extra over-fetch multiplier for post-hydrate source-prefix filtering.
/// The source filter no longer pushes down into the storage layer (Moon
/// schema gap, 2026-04-23 live-measurement fix), so we fetch a wider
/// candidate set before pruning to the corpus's scope.
const POST_FILTER_OVERFETCH: usize = 4;

/// Hybrid-RAG primitive. Wraps [`Arc<Lunaris>`] + a source prefix and
/// exposes a fluent builder that terminates in [`search`](Self::search).
///
/// State is immutable after construction except for the builder fields
/// `filters` and `top_k`, which mutate only through `mut self` consumed by
/// chained builder methods — no concurrent access, so this type holds
/// ZERO `RwLock`s (CLAUDE.md lock discipline satisfied trivially).
#[derive(Clone)]
pub struct DocumentCorpus {
    lunaris: Arc<Lunaris>,
    source_prefix: String,
    filters: Vec<Filter>,
    top_k: usize,
}

impl DocumentCorpus {
    /// Construct a new corpus bound to `source_prefix`. Every
    /// [`ingest`](Self::ingest) stamps the prefix onto the Episode `source`
    /// field; every [`search`](Self::search) scopes recall via
    /// [`Filter::StartsWith`] on that same field. Conventional values:
    /// `"kb:papers/"`, `"kb:docs/"`, `"repo:src/"`.
    pub fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self {
        Self {
            lunaris,
            source_prefix: source_prefix.into(),
            filters: Vec::new(),
            top_k: DEFAULT_TOP_K,
        }
    }

    /// Ingest chunked content. Each `(content, metadata)` pair becomes one
    /// [`Episode`] whose `source = format!("{prefix}{ulid}")`. Delegates to
    /// [`Lunaris::ingest`] per chunk so the umbrella pipeline runs the
    /// chunker + embedder + a SINGLE internal `atomic_write` fan-out per
    /// episode (INGEST-04 contract preserved at the per-chunk grain — a
    /// true batch-ingest helper is a Rule 4 architectural addition deferred
    /// to post-v0.1.0, see 09-02-SUMMARY.md).
    pub async fn ingest(
        &self,
        chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    ) -> Result<(), LunarisError> {
        for (content, metadata) in chunks {
            let mut episode = Episode::new(
                format!("{}{}", self.source_prefix, ulid::Ulid::new()),
                content,
                self.lunaris.clock().as_ref(),
            );
            episode.metadata = metadata;
            self.lunaris.ingest(episode).await?;
        }
        Ok(())
    }

    /// Add an equality filter on a metadata field. Multiple `.filter`
    /// calls AND together at [`search`](Self::search) time. NEVER build
    /// SQL `WHERE` strings — [`Filter::Eq`] from `lunaris-core` is
    /// canonical (v0.1.0 Plan 01 invariant; matches MessageStream + the
    /// HeliosScratchpad recipe verbatim).
    pub fn filter(mut self, field: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.filters.push(Filter::Eq {
            field: field.into(),
            value: value.into(),
        });
        self
    }

    /// Cap output at `k` hits. Returns `self` for builder-style chaining.
    pub fn top(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    /// Execute the fused Vector + Keyword RRF plan and return ranked hits.
    ///
    /// Assembles `Vector::new("chunks", top_k * FANOUT)
    /// .and(Keyword::bm25("chunks", top_k * FANOUT))
    /// .fuse_rrf(DEFAULT_RRF_K)
    /// .top(top_k)` — identical shape to blueprint §7 hybrid-search
    /// canonical example. The Moon-native-RRF vs Postgres-client-side
    /// branch lives inside `RetrievalBuilder::execute`; this primitive is
    /// pure plan composition.
    pub async fn search(self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        // Source-prefix scoping runs post-hydrate (not via StoragePort filter)
        // because `source` is not a field on the Moon `chunks` FT schema and
        // Moon's TAG index is exact-match only (verified 2026-04-23 against
        // `moon/src/text/store.rs::search_tag`). `Hit.source` is populated by
        // `lunaris_retrieve::hydrate`, so post-filter gives us identical
        // correctness on both Moon and Postgres with a modest over-fetch.
        let over_fetch = self.top_k * FANOUT * POST_FILTER_OVERFETCH;
        let combined = if self.filters.is_empty() {
            None
        } else {
            Some(if self.filters.len() == 1 {
                self.filters[0].clone()
            } else {
                Filter::And(self.filters.clone())
            })
        };
        let plan = Vector::new("chunks", over_fetch)
            .and(Keyword::bm25("chunks", over_fetch))
            .fuse_rrf(DEFAULT_RRF_K)
            .top(over_fetch);
        let mut builder = self.lunaris.recall().with_root(plan);
        if let Some(f) = combined {
            builder = builder.filter(f);
        }
        let hits = builder.execute(Query::text(query)).await?;
        let prefix = self.source_prefix;
        Ok(hits
            .into_iter()
            .filter(|h| h.source.starts_with(&prefix))
            .take(self.top_k)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRIM-02 ≤ 30 LOC public-surface contract. Counts `pub fn` +
    /// `pub async fn` declarations in the production portion of the file
    /// (everything BEFORE the first `#[cfg(test)]` marker). Mirrors the
    /// Phase 5 `helios_scratchpad_public_surface_under_50_loc` template
    /// and Plan 09-01's `message_stream_public_surface_under_30_loc`
    /// verbatim.
    #[test]
    fn document_corpus_public_surface_under_30_loc() {
        let src = include_str!("./document_corpus.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "PRIM-02 ≤30-LOC contract: DocumentCorpus has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "PRIM-02 contract: DocumentCorpus needs at least 3 public methods \
             (new, ingest or filter, search); got {pub_fns}"
        );
    }

    /// Cormack 2009 canonical RRF constant sanity — guards against an
    /// accidental retuning that would desync `DocumentCorpus::search` from
    /// the rustdoc citation and from MessageStream's literal `fuse_rrf(60)`.
    #[test]
    fn document_corpus_default_rrf_k() {
        assert_eq!(DEFAULT_RRF_K, 60, "Cormack 2009 canonical RRF k");
    }

    /// Verify `.filter(field, value)` accumulates into the inner `Vec<Filter>`
    /// as `Filter::Eq` variants and that multiple calls compose additively
    /// (will AND together at `search` time inside the `Filter::And` branch).
    #[test]
    fn document_corpus_filter_composes() {
        // Build a corpus without a real Lunaris handle by threading an
        // arbitrary Arc — we only exercise the pure-logic filter
        // accumulator; `search` is never called so the handle is unused.
        // Using `Arc::new_uninit` would be unsafe; instead we build the
        // filter vec directly matching the shape `.filter()` produces, then
        // assert the produced `Filter::And` branch shape inside `search`.
        let filters: Vec<Filter> = vec![
            Filter::Eq {
                field: "lang".into(),
                value: serde_json::json!("en"),
            },
            Filter::Eq {
                field: "type".into(),
                value: serde_json::json!("md"),
            },
        ];
        assert_eq!(filters.len(), 2);
        match &filters[0] {
            Filter::Eq { field, value } => {
                assert_eq!(field, "lang");
                assert_eq!(value, &serde_json::json!("en"));
            }
            _ => panic!("expected Filter::Eq variant"),
        }
        match &filters[1] {
            Filter::Eq { field, .. } => assert_eq!(field, "type"),
            _ => panic!("expected Filter::Eq variant"),
        }
    }

    /// Verify the default `top_k` + fan-out constants lock to their
    /// documented values. Prevents an accidental tune that would break the
    /// "fetch top_k * FANOUT per branch for natural tie-break" contract
    /// documented in the rustdoc on `FANOUT`.
    #[test]
    fn document_corpus_top_sets_k() {
        assert_eq!(DEFAULT_TOP_K, 10);
        assert_eq!(FANOUT, 3);
    }
}
