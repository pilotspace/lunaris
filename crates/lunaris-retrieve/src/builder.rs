//! `RetrievalBuilder` — the umbrella API surface that `Lunaris::recall()` returns.
//!
//! Wires the operator root + the four required Arcs (storage, embedder,
//! keyword, clock-substitute via stored Hlc) into a `tower::Service`-shaped
//! retriever. Terminal `.execute(query)` runs the tree and hydrates.
//!
//! ## Pattern — Vector + Keyword (Phase 2)
//!
//! ```ignore
//! let hits = lunaris.recall()
//!     .with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))
//!     .filter_str("source LIKE 'helios:fs/'").unwrap()
//!     .execute(Query::text("brown fox"))
//!     .await?;
//! ```
//!
//! ## Pattern — Graph-anchored recall (Plan 03-02)
//!
//! The canonical blueprint §8 example composes a Vector branch with a
//! Graph branch via the same `fuse_rrf` operator. The Graph branch takes
//! pre-resolved [`crate::EntityId`]s (from the RETRIEVE-13 planner stub
//! per D-13) and walks the graph for `hops` BFS steps:
//!
//! ```ignore
//! let hits = lunaris.recall()
//!     .with_root(
//!         Vector::new("chunks", 30)
//!             .and(Graph::anchored(query_entity_ids, 2))
//!             .fuse_rrf(60)
//!             .rerank(handle.reranker())
//!             .top(5))
//!     .execute(Query::text("brown fox"))
//!     .await?;
//! ```
//!
//! `Graph::anchored(_, hops)` clamps `hops` to `[1, MAX_GRAPH_HOPS=5]`
//! (D-14 DoS defense). Empty `entity_ids` short-circuits to an empty
//! result without touching storage. The Cypher template is portable across
//! Moon GRAPH.QUERY and Postgres AGE per D-16; W-7 alignment uses
//! `id_hex` as the match property.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lunaris_core::storage::types::Filter;
use lunaris_core::{Embedder, Hlc, KeywordPort, LunarisError, Scope, StoragePort};
use ulid::Ulid;

use crate::operators::recency::{RecencyConfig, rescore_recency};

// Plan 03-02: Re-export Graph constants alongside the builder so callers
// constructing a `recall()` chain can reach the hop / k defaults via the
// same `lunaris_retrieve::*` import surface.
pub use crate::operators::graph::{DEFAULT_GRAPH_HOPS, DEFAULT_GRAPH_K, Graph, MAX_GRAPH_HOPS};

use crate::hydrate::hydrate;
use crate::operators::modifiers::FilterParseError;
use crate::operators::vector::Vector;
use crate::operators::{QueryContext, Retriever};
use crate::types::RawHit;
use crate::types::{Hit, Query};

/// Builder returned by `Lunaris::recall()`.
#[must_use = "RetrievalBuilder must be terminated with .vector(..) / .keyword(..) / .graph(..) and awaited to actually retrieve hits"]
pub struct RetrievalBuilder {
    pub(crate) root: Box<dyn Retriever>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) storage: Arc<dyn StoragePort>,
    pub(crate) keyword: Arc<dyn KeywordPort>,
    pub(crate) moon_storage: Option<Arc<lunaris_storage_moon::MoonStorage>>,
    pub(crate) base_filter: Option<Filter>,
    pub(crate) base_as_of: Option<Hlc>,
    /// Wave 2.5A/2.5C: scope for this retrieval call. Seeded by
    /// `ScopedLunaris::recall()` / `::dsl()` with the bound scope; bare
    /// `Lunaris::recall()` seeds `Scope::dev()` with a `tracing::warn!` for
    /// backwards compat — adopters should migrate to `engine.scoped(s).recall()`.
    pub(crate) scope: Scope,
    /// Plan 04-04 B-9 fix: when `true`, hydration ORs this into every
    /// `Hit::degraded`. Set by `Lunaris::recall_with_degraded_check` when the
    /// verifier queue depth crosses `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD`.
    pub(crate) initial_degraded: bool,
    /// P0 #3 Wave 1: optional post-hydrate recency rescorer. When `Some`,
    /// `execute()` calls `rescore_recency` after `hydrate()` and before
    /// returning hits. Default `None` preserves pre-recency ordering for
    /// every caller that doesn't opt in.
    pub(crate) recency_config: Option<RecencyConfig>,
    /// Phase 14.2: optional per-handle boost cache threaded in by
    /// `Lunaris::recall()`. When `Some`, `execute()` snapshots the cache
    /// under a read lock (guard dropped before any `.await`), applies the
    /// delta to matching hits, and re-sorts. `None` = no boost pass (default,
    /// preserves pre-14.2 ordering for callers that construct a builder
    /// without going through `Lunaris::recall()`).
    #[allow(clippy::type_complexity)]
    pub(crate) boost_cache: Option<Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>>>,
    /// ADD task activation-ledger — optional persistent activation-ledger
    /// boost source. When `Some`, `execute()` calls `provider.priors(..)`
    /// ONCE (a single batched storage read) after hydrate and adds the
    /// returned prior to each matching hit's score, BEFORE the (unchanged)
    /// `boost_cache` LRU delta pass. `None` (the default) disables this
    /// pass entirely — no behavior change for callers that never opt in.
    pub(crate) boost_provider: Option<Arc<dyn crate::boost_provider::BoostProvider>>,
}

impl RetrievalBuilder {
    /// Construct a fresh builder. Default root = `Vector::new("chunks", 30)`.
    ///
    /// Wave 2.5A: `scope` defaults to `Scope::dev()` here. Callers that want
    /// real scope-isolation should use `with_scope(scope)` or go through
    /// `ScopedLunaris::dsl()` which pre-seeds the scope automatically.
    pub fn new(
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        Self {
            root: Box::new(Vector::new("chunks", 30)),
            storage,
            keyword,
            embedder,
            moon_storage: None,
            base_filter: None,
            base_as_of: None,
            // scope-dev-allowed: bare-recall-default — warn-on-execute when
            // scope==Scope::dev(); ScopedLunaris::dsl() overrides via with_scope().
            scope: Scope::dev(),
            initial_degraded: false,
            recency_config: None,
            boost_cache: None,
            boost_provider: None,
        }
    }

    /// Convenience constructor used by `Lunaris::recall()`. Same as `new`
    /// but takes the order callers are likely to think of (storage, keyword,
    /// embedder).
    pub fn from_handle(
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        Self::new(storage, keyword, embedder)
    }

    /// Wave 2.5C: set the scope for this retrieval call.
    ///
    /// `ScopedLunaris::recall()` and `::dsl()` call this automatically so
    /// scope-bound callers don't need to. Bare `Lunaris::recall()` keeps
    /// `Scope::dev()` as the default with a `tracing::warn!` in `execute`.
    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    /// Wire a typed `MoonStorage` Arc into the builder so the Phase 1.5
    /// `fuse_rrf` Moon-native dispatch path can fire. The umbrella
    /// `Lunaris::recall()` calls this when the handle was opened against
    /// a `moon://` URL — caller code never needs to invoke this directly.
    pub fn with_moon_storage(mut self, moon: Arc<lunaris_storage_moon::MoonStorage>) -> Self {
        self.moon_storage = Some(moon);
        self
    }

    /// Replace the root retriever.
    ///
    /// `with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))`.
    pub fn with_root<R: Retriever + 'static>(mut self, root: R) -> Self {
        self.root = Box::new(root);
        self
    }

    /// Use a pre-built `Box<dyn Retriever>` as the root.
    pub fn with_root_boxed(mut self, root: Box<dyn Retriever>) -> Self {
        self.root = root;
        self
    }

    /// Set the structured filter that will narrow every operator's candidate
    /// set. Overrides any previous filter.
    pub fn filter(mut self, f: Filter) -> Self {
        self.base_filter = Some(f);
        self
    }

    /// Parse a v0 string DSL into a [`Filter`] and set it. Returns the parser
    /// error at builder time so callers see invalid syntax before any IO.
    pub fn filter_str(mut self, s: &str) -> Result<Self, FilterParseError> {
        let f = crate::operators::modifiers::filter_str(s)?;
        self.base_filter = Some(f);
        Ok(self)
    }

    /// Set the bi-temporal `as_of` snapshot timestamp.
    pub fn as_of(mut self, ts: Hlc) -> Self {
        self.base_as_of = Some(ts);
        self
    }

    /// Plan 04-04 B-9 fix: caller-set degraded flag plumbed through hydration.
    /// `Lunaris::recall_with_degraded_check` sets this when the verifier
    /// queue depth crosses `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` so every
    /// returned `Hit::degraded` is `true` (VERIFY-06 backpressure surface).
    ///
    /// Returns `self` so callers can chain into the rest of the builder.
    pub fn with_initial_degraded(mut self, deg: bool) -> Self {
        self.initial_degraded = deg;
        self
    }

    /// Wrap an upstream operator with `top(n)` at builder time. This is a
    /// convenience over `.with_root(prev_root.top(n))` — you can also call
    /// `.top(n)` directly on the operator before passing to `with_root`.
    pub fn top(self, n: usize) -> Self {
        let new_root = crate::operators::modifiers::TopRetriever::new(self.root, n);
        Self { root: Box::new(new_root), ..self }
    }

    /// Plan 02-03: Wrap the current root with a [`crate::RerankRetriever`].
    /// The cross-encoder reorders the top-30 (default) candidates and tags
    /// every hit with `rerank_applied = reranker.applies()` so callers can
    /// see whether the real model fired (vs the noop fallback).
    pub fn rerank(self, reranker: Arc<dyn lunaris_rerank::Reranker>) -> Self {
        let new_root = crate::operators::rerank::RerankRetriever::new(self.root, reranker);
        Self { root: Box::new(new_root), ..self }
    }

    /// W5 task 1: wrap the current root with a [`crate::RerankRetriever`]
    /// configured with an abstention gate. Hits scoring below `min_score`
    /// on the cross-encoder's sigmoid output (∈ `[0,1]` for
    /// `bge-reranker-v2-m3`) are dropped instead of being returned as
    /// "best of the worst" — an empty result is a legitimate abstention
    /// signal. Uses the default top-in ([`crate::DEFAULT_RERANK_TOP_IN`]);
    /// use `.rerank(reranker)` + `RerankRetriever::with_top_in(..)` directly
    /// (via `.with_root_boxed`) when both a custom `k_in` AND a threshold
    /// are needed.
    ///
    /// ```ignore
    /// let hits = lunaris.recall()
    ///     .rerank_with_threshold(handle.reranker(), 0.5)
    ///     .top(5)
    ///     .execute(Query::text("what did we decide about pricing?"))
    ///     .await?;
    /// // hits.is_empty() == true means "no confident memory" (abstain), not
    /// // "reranker returned zero-relevance results ranked anyway".
    /// ```
    pub fn rerank_with_threshold(
        self,
        reranker: Arc<dyn lunaris_rerank::Reranker>,
        min_score: f32,
    ) -> Self {
        let new_root = crate::operators::rerank::RerankRetriever::new(self.root, reranker)
            .with_min_score(min_score);
        Self { root: Box::new(new_root), ..self }
    }

    /// Plan 02-03: Wrap the current root with a [`crate::DegradedFallbackRetriever`].
    /// On any error from the current (primary) root, the operator switches
    /// to `fallback` and tags returned hits with `degraded: true`.
    pub fn degraded_fallback<R: Retriever + 'static>(self, fallback: R) -> Self {
        let new_root = crate::operators::degraded::DegradedFallbackRetriever::new(
            self.root,
            Box::new(fallback),
        );
        Self { root: Box::new(new_root), ..self }
    }

    /// N5/B2: Replace the root with a RAPTOR [`crate::operators::tree::Tree`]
    /// operator targeting the `communities` vector index with `k` top summary
    /// nodes and the given BFS descent `depth` (clamped to
    /// [`crate::operators::tree::MAX_TREE_DEPTH`]).
    ///
    /// The operator vector-searches community summaries, then descends
    /// `Community.members` links to collect leaf-chunk IDs. Downstream
    /// `hydrate()` resolves them into full `Hit`s. Whole-document and
    /// multi-hop questions that flat chunk retrieval misses at small `k`
    /// benefit from this path.
    ///
    /// ```ignore
    /// let hits = lunaris.recall()
    ///     .tree("communities", 5, 1)
    ///     .execute(Query::text("What are the main themes?"))
    ///     .await?;
    /// ```
    pub fn tree(mut self, index: impl Into<String>, k: usize, depth: usize) -> Self {
        let root = crate::operators::tree::Tree::new(index, k).with_depth(depth);
        self.root = Box::new(root);
        self
    }

    /// P0 #3 Wave 1: opt into post-hydrate recency rescoring.
    ///
    /// After `hydrate()` produces the final `Vec<Hit>`, [`rescore_recency`]
    /// applies the configured [`RecencyConfig`] (Exp half-life or ActR by
    /// default; pluggable via [`crate::RecencyScorer`]) and re-sorts in
    /// descending fused-score order. `now` is `query.as_of` when set (so
    /// bi-temporal replay stays temporally consistent), otherwise wall-clock
    /// system time.
    ///
    /// ```ignore
    /// use std::time::Duration;
    /// use lunaris_retrieve::{Exp, RecencyConfig, TimeSource};
    ///
    /// let hits = lunaris.recall()
    ///     .recency(RecencyConfig::new(
    ///         TimeSource::ValidFrom,
    ///         Box::new(Exp::new(Duration::from_secs(86_400 * 7))),
    ///     ))
    ///     .execute(Query::text("acme commitments"))
    ///     .await?;
    /// ```
    pub fn recency(mut self, config: RecencyConfig) -> Self {
        self.recency_config = Some(config);
        self
    }

    /// Phase 14.2: wire the per-handle boost cache into this builder.
    ///
    /// Called automatically by `Lunaris::recall()` so callers using
    /// `engine.scoped(scope).recall()` / `.dsl()` get the boost pass for
    /// free. Callers constructing a builder directly via [`Self::new`] /
    /// [`Self::from_handle`] can opt in explicitly.
    ///
    /// `None` (the default) disables the post-hydrate boost pass entirely,
    /// preserving the pre-14.2 ordering contract.
    pub fn with_boost_cache(
        mut self,
        cache: Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>>,
    ) -> Self {
        self.boost_cache = Some(cache);
        self
    }

    /// ADD task activation-ledger — wire a persistent [`crate::boost_provider::BoostProvider`]
    /// into this builder's post-hydrate boost pass.
    ///
    /// NOT called by default `Lunaris::recall()` (frozen contract: opt-in
    /// only, so the sub-25ms core recall path cannot regress for existing
    /// callers). Composes with [`Self::with_boost_cache`] — the provider's
    /// prior applies first, the LRU delta applies after, unchanged.
    pub fn with_boost_provider(
        mut self,
        provider: Arc<dyn crate::boost_provider::BoostProvider>,
    ) -> Self {
        self.boost_provider = Some(provider);
        self
    }

    /// Run the tree and hydrate. Terminal — consumes the builder.
    pub async fn execute(mut self, mut query: Query) -> Result<Vec<Hit>, LunarisError> {
        // P0 #1 Wave 2 — warn when the builder is still seeded with the
        // bare-recall `Scope::dev()` default. ScopedLunaris::dsl() / .recall()
        // both call `with_scope(self.scope)` so the warn only fires on the
        // unscoped `Lunaris::recall().execute(...)` path (the documented
        // backward-compat shim). One warn per execute is sufficient — query
        // throughput already aggregates upstream of this call.
        // scope-dev-allowed: bare-recall-default-detector — equality check + warn
        // message both reference Scope::dev() for the detector itself.
        if self.scope == Scope::dev() {
            // scope-dev-allowed: warn-message-text — message references the
            // marker for operator readability.
            tracing::warn!(
                "RetrievalBuilder using Scope::dev() default — migrate to engine.scoped(scope).dsl()"
            );
        }
        // Builder-level filter / as_of override the per-query fields IFF the
        // query didn't already set them. Builder fields are the persistent
        // "this handle's defaults"; per-query fields are the one-shot
        // override.
        if query.filter.is_none() {
            query.filter = self.base_filter.clone();
        }
        if query.as_of.is_none() {
            query.as_of = self.base_as_of;
        }
        // W5 task 2 (AS_OF pinning): if the caller supplied NO as_of at all
        // (neither `Query::as_of` nor the builder's `.as_of(ts)` default),
        // pin ONE snapshot timestamp HERE — at the root of this retrieve
        // execution, before `QueryContext` is constructed — and thread it
        // through `query.as_of` for the entire fan-out.
        //
        // Without this, every operator/helper that defaults a missing
        // `as_of` (`Tree`'s BFS descent, `rerank`'s `partial_hydrate_text`,
        // `hydrate()`'s final chunk+episode pass) independently calls
        // `HlcClock::new(0).tick()` at whatever wall-clock instant it
        // happens to run. A single fused tree with multiple branches can
        // then straddle a concurrent invalidation: one branch reads
        // pre-supersede facts, another reads post-supersede facts, in the
        // SAME logical query. Pinning once here means every downstream
        // `ctx.query.as_of.unwrap_or_else(|| live_clock.tick())` fallback
        // never fires — they all observe the identical `Some(pinned)`.
        //
        // A caller-supplied as_of (explicit `Query { as_of: Some(_), .. }`
        // or `.as_of(ts)` on the builder) is NEVER touched — this branch
        // only runs when both are `None`.
        if query.as_of.is_none() {
            query.as_of = Some(lunaris_core::HlcClock::new(0).tick());
        }
        let as_of = query.as_of;
        // Plan 04-04 B-9: snapshot the initial_degraded flag BEFORE moving
        // the rest of the builder into the QueryContext / hydrate calls.
        let initial_degraded = self.initial_degraded;
        let scope = self.scope.clone();
        // P0 #3 Wave 1: snapshot the recency config BEFORE moving self into
        // the QueryContext. None = recency pass disabled (default).
        let recency_config = self.recency_config;
        // Phase 14.2: snapshot the boost cache Arc BEFORE moving self into
        // the QueryContext. None = boost pass disabled.
        let boost_cache = self.boost_cache.take();
        // ADD task activation-ledger: snapshot the boost provider Arc BEFORE
        // moving self into the QueryContext. None = provider pass disabled
        // (the default — see `with_boost_provider` doc).
        let boost_provider = self.boost_provider.take();
        let ctx = match self.moon_storage.clone() {
            Some(moon) => QueryContext::with_moon(
                query,
                scope,
                self.embedder,
                self.storage.clone(),
                self.keyword,
                moon,
            ),
            None => {
                QueryContext::new(query, scope, self.embedder, self.storage.clone(), self.keyword)
            }
        };
        let raw = self.root.retrieve(&ctx).await?;
        let mut hits =
            hydrate(self.storage.as_ref(), &ctx.scope, raw, as_of, initial_degraded).await?;
        if let Some(cfg) = recency_config {
            // `as_of` wins as the recency anchor when set, so bi-temporal
            // replay stays temporally consistent ("recency at that moment").
            // Otherwise use wall-clock now. node_id/counter are 0 since the
            // recency math only reads `wall_ms`.
            let now = as_of.unwrap_or_else(|| {
                let wall_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Hlc::from_parts(wall_ms, 0, 0)
            });
            rescore_recency(&mut hits, now, &cfg);
        }

        // ADD task activation-ledger: post-hydrate PERSISTENT boost-provider
        // pass. Runs BEFORE the (untouched) Phase 14.2 LRU pass below so the
        // two compose as "provider prior, then LRU delta" per the frozen
        // contract. A single batched `provider.priors(..)` call covers the
        // whole hit set — never one read per hit.
        if let Some(provider) = boost_provider {
            let ids: Vec<Ulid> = hits
                .iter()
                .filter_map(|h| <[u8; 16]>::try_from(h.id.as_slice()).ok().map(Ulid::from_bytes))
                .collect();
            if !ids.is_empty() {
                let priors = provider.priors(&ctx.scope, &ids).await;
                if !priors.is_empty() {
                    for hit in &mut hits {
                        if let Ok(arr) = <[u8; 16]>::try_from(hit.id.as_slice()) {
                            let id = Ulid::from_bytes(arr);
                            if let Some(&prior) = priors.get(&id) {
                                hit.score += prior;
                            }
                        }
                    }
                    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
                }
            }
        }

        // Phase 14.2: post-hydrate boost pass.
        //
        // Snapshot the cache under a read lock — guard is dropped before any
        // subsequent `.await` (there are none after this block).  `peek` is
        // used deliberately so consulting the cache does NOT update LRU
        // recency; only `end_turn` writes (via `put`) should advance recency.
        if let Some(cache) = boost_cache {
            let snapshot: HashMap<Ulid, f32> = {
                let guard = cache.read();
                hits.iter()
                    .filter_map(|h| {
                        let id = Ulid::from_bytes(h.id.as_slice().try_into().ok()?);
                        guard.peek(&(ctx.scope.clone(), id)).map(|&v| (id, v))
                    })
                    .collect()
            }; // read guard dropped here — no .await follows

            if !snapshot.is_empty() {
                for hit in &mut hits {
                    if let Ok(arr) = <[u8; 16]>::try_from(hit.id.as_slice()) {
                        let id = Ulid::from_bytes(arr);
                        if let Some(&delta) = snapshot.get(&id) {
                            hit.score += delta;
                        }
                    }
                }
                hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
            }
        }

        Ok(hits)
    }

    /// Run the tree and return the unhydrated [`RawHit`]s. Terminal —
    /// consumes the builder.
    ///
    /// `execute()` runs the operator tree AND `hydrate()`, which looks up
    /// each result's `id` as a `lunaris:chunk:<ulid>` row and silently
    /// drops anything that doesn't decode (since-deleted chunks, OR — the
    /// motivating case — recall over a non-chunk index like `facts` /
    /// `entities` / `communities` whose ids are NOT chunk ULIDs). Bench
    /// harnesses that just want to measure search-path latency and don't
    /// need full chunk text/episode metadata can call this method to
    /// bypass the cull. Production callers should keep using `execute()`
    /// so the contract "hits always have chunk text" holds.
    ///
    /// Gap 9 follow-up 2026-04-21 — added so `recall_hot_path` can run
    /// against the bench-seeded `facts` corpus without `hydrate` filtering
    /// every hit out.
    pub async fn execute_raw(self, mut query: Query) -> Result<Vec<RawHit>, LunarisError> {
        if query.filter.is_none() {
            query.filter = self.base_filter.clone();
        }
        if query.as_of.is_none() {
            query.as_of = self.base_as_of;
        }
        // W5 task 2 (AS_OF pinning) — same seam as `execute()`. `execute_raw`
        // skips `hydrate()` but the operator tree (Tree/rerank/Graph/etc.)
        // still needs a single pinned instant threaded through `ctx.query.as_of`.
        if query.as_of.is_none() {
            query.as_of = Some(lunaris_core::HlcClock::new(0).tick());
        }
        let scope = self.scope.clone();
        let ctx = match self.moon_storage.clone() {
            Some(moon) => QueryContext::with_moon(
                query,
                scope,
                self.embedder,
                self.storage.clone(),
                self.keyword,
                moon,
            ),
            None => {
                QueryContext::new(query, scope, self.embedder, self.storage.clone(), self.keyword)
            }
        };
        self.root.retrieve(&ctx).await
    }
}
