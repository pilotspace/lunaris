//! `Lunaris` — the high-level memory-engine handle (Phase 2 surface).
//!
//! Wraps `Arc<dyn StoragePort> + Arc<dyn Embedder> + Arc<HlcClock>` so callers
//! can construct multiple instances against different URLs (Plan 02-04 needs
//! this for the Moon-vs-Postgres benches). All three fields are `Arc`-shared
//! so `Lunaris::clone()` is cheap and `Lunaris` is `Send + Sync` for free.
//!
//! ## Construction paths
//!
//! - [`Lunaris::open`] — production constructor. Routes the `url` through the
//!   Phase 1 [`crate::open::open`] dispatcher to pick a [`StoragePort`] backend,
//!   constructs the default embedder ([`lunaris_embed::CandleEmbeddingGemma`]
//!   under the `candle` feature) and a fresh `HlcClock(node_id=0)`.
//! - [`Lunaris::with_parts`] — escape hatch for tests + the Plan 02-01
//!   latency-budget swap. Lets callers wire any `Arc<dyn StoragePort>` and
//!   `Arc<dyn Embedder>` directly. Used by the Phase 2 ingest smoke test
//!   (in-memory recording storage + `StubEmbedder`).
//! - [`Lunaris::with_embedder`] — public escape hatch to replace the
//!   embedder on an already-constructed handle (e.g., swap from candle to
//!   `OllamaEmbedder` if the per-batch budget busts).
//!
//! ## Invariant
//!
//! `Lunaris` does NOT cache any per-call state. Every call constructs a fresh
//! borrow of the three Arcs, so the same handle is safe to use from multiple
//! tokio tasks concurrently.

use std::sync::Arc;

use lunaris_consolidate::{Consolidator, NoopConsolidator};
use lunaris_core::{Embedder, HlcClock, KeywordPort, LunarisError, StoragePort};
use lunaris_extract::{Extractor, NoopExtractor};
use lunaris_rerank::{NoopReranker, Reranker};
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_postgres::PostgresStorage;
use lunaris_verify::{NoopVerifier, Verifier};

use crate::consolidator_pipeline::ConsolidatorPipelineHandle;
use crate::graph_pipeline::GraphPipelineHandle;
use crate::verify_pipeline::VerifierPipelineHandle;

#[derive(Clone)]
pub struct Lunaris {
    pub(crate) storage: Arc<dyn StoragePort>,
    pub(crate) keyword: Arc<dyn KeywordPort>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) clock: Arc<HlcClock>,
    /// Concrete `MoonStorage` Arc when the handle was opened against a `moon://` URL.
    /// Plan 02-02's `fuse_rrf` Moon-native dispatch reads this to opt into the
    /// one-round-trip `text().hybrid_search()` path. None for Postgres / custom backends.
    pub(crate) moon_storage: Option<Arc<MoonStorage>>,
    /// Plan 02-03: cross-encoder reranker for the recall hot path.
    /// Defaults to `BgeRerankerV2M3` when `~/.cache/lunaris/models/bge-reranker-v2-m3/`
    /// is present; falls back to `NoopReranker` per RETRIEVE-06 contract when
    /// the cache is missing. Callers swap via `with_reranker(reranker)`.
    pub(crate) reranker: Arc<dyn Reranker>,
    /// Plan 03-03: graph extraction pipeline toggle (D-10/D-11). Default OFF.
    /// The `Extractor` itself lives INSIDE the handle's
    /// `RwLock<Option<Arc<dyn Extractor>>>` — callers `swap` via
    /// [`Self::with_extractor`] which delegates to
    /// [`GraphPipelineHandle::set_extractor`]; toggle ON/OFF via
    /// `handle.graph_pipeline().enable() / .disable()` (D-10 single-switch
    /// surface, EXTRACT-06).
    pub(crate) graph_pipeline: Arc<GraphPipelineHandle>,
    /// Plan 04-04: slow-path Verifier worker toggle (D-08, default OFF per
    /// blueprint §5.1). Owns the `Arc<dyn Verifier>`, the late-bound
    /// `Arc<dyn StoragePort>`, the worker JoinHandle, and the shutdown
    /// `tokio::sync::Notify`. Toggle ON/OFF via
    /// `handle.verify_pipeline().enable() / .disable()` (D-08 single-switch
    /// surface, VERIFY-01..06).
    pub(crate) verify_pipeline: Arc<VerifierPipelineHandle>,
    /// Plan 04-04: ACT-R Consolidator worker toggle (D-08, default OFF per
    /// blueprint §5.1). Same shape as `verify_pipeline`. Toggle ON/OFF via
    /// `handle.consolidator_pipeline().enable() / .disable()` (D-08
    /// single-switch surface, CONSOL-01..05).
    pub(crate) consolidator_pipeline: Arc<ConsolidatorPipelineHandle>,
}

impl std::fmt::Debug for Lunaris {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lunaris")
            .field("backend_capabilities", &self.storage.capabilities())
            .field("embedder_dim", &self.embedder.dim())
            .field("clock_node_id", &self.clock.node_id())
            .field("has_moon_native_path", &self.moon_storage.is_some())
            .field("reranker_applies", &self.reranker.applies())
            .field("graph_pipeline_enabled", &self.graph_pipeline.is_enabled())
            .field("verify_pipeline_enabled", &self.verify_pipeline.is_enabled())
            .field(
                "consolidator_pipeline_enabled",
                &self.consolidator_pipeline.is_enabled(),
            )
            .finish()
    }
}

impl Lunaris {
    /// Production constructor. Opens a storage backend by URL and constructs
    /// the default embedder.
    ///
    /// - `moon://...` → [`lunaris_storage_moon::MoonStorage`] backend.
    ///   Plan 02-02 wires the typed `Arc<MoonStorage>` alongside the dyn
    ///   trait Arcs so `recall().fuse_rrf()` can take the Moon-native one-
    ///   round-trip path.
    /// - `postgres://...` → [`lunaris_storage_postgres::PostgresStorage`] backend.
    ///   `moon_storage` field stays `None`; `recall().fuse_rrf()` falls back
    ///   to client-side reciprocal rank fusion.
    /// - default embedder under `candle` feature: `lunaris_embed::CandleEmbeddingGemma`
    ///   with default cache path `~/.cache/lunaris/models/embedding-gemma-300m/`.
    ///   When the cache is missing the constructor surfaces the actionable
    ///   `embedding-gemma weights missing at PATH — run huggingface-cli ...`
    ///   error from `CandleEmbeddingGemma::new`.
    /// - default embedder under `ollama`-only feature build:
    ///   `lunaris_embed::OllamaEmbedder` pointing at `http://localhost:11434`.
    /// - default embedder when neither feature is on: returns
    ///   `LunarisError::Storage(StorageError::NotSupported(...))` — callers
    ///   in this configuration MUST use [`Self::with_parts`] with their own
    ///   embedder.
    pub async fn open(url: &str) -> Result<Self, LunarisError> {
        let scheme = url.split("://").next().unwrap_or("");
        let embedder = default_embedder().await?;
        let clock = HlcClock::new(0);
        let reranker = default_reranker().await;
        // Plan 03-03: Construct the graph pipeline handle. Initial state
        // comes from `LUNARIS_GRAPH_ENABLED=1|0` env var (D-10); default OFF
        // per blueprint §5.2. The default extractor is candle Gemma-3 4B (or
        // NoopExtractor on cache miss — see `default_extractor`).
        let extractor = default_extractor().await;
        let initial_graph_state = GraphPipelineHandle::initial_state_from_env();
        let graph_pipeline = Arc::new(GraphPipelineHandle::new(initial_graph_state, extractor));
        // Plan 04-04: Construct the verifier + consolidator pipeline handles.
        // Initial state from `LUNARIS_VERIFY_ENABLED` / `LUNARIS_CONSOLIDATE_ENABLED`
        // env vars (D-08); default OFF per blueprint §5.1. Default backends
        // are NoopVerifier / NoopConsolidator — production callers wire real
        // backends via `with_verifier` / `with_consolidator`.
        let verifier = default_verifier().await;
        let consolidator = default_consolidator();
        let initial_verify_state = VerifierPipelineHandle::initial_state_from_env();
        let initial_consolidate_state = ConsolidatorPipelineHandle::initial_state_from_env();
        let verify_pipeline = Arc::new(VerifierPipelineHandle::new(
            initial_verify_state,
            verifier,
        ));
        let consolidator_pipeline = Arc::new(ConsolidatorPipelineHandle::new(
            initial_consolidate_state,
            consolidator,
        ));
        match scheme {
            "moon" => {
                let m = Arc::new(MoonStorage::connect(url).await?);
                let storage_arc: Arc<dyn StoragePort> = m.clone();
                // B-10: bind the StoragePort Arc to BOTH pipelines AFTER
                // we've constructed it. Also bind the HlcClock so the
                // Plan 04-04 Task 4 apply_supersede has a tick source. If
                // env var initial-state was ON, also kick the worker via
                // spawn_worker_if_idle so callers don't have to call
                // enable() a second time post-bind.
                verify_pipeline.bind_storage(storage_arc.clone());
                verify_pipeline.bind_clock(clock.clone());
                consolidator_pipeline.bind_storage(storage_arc.clone());
                if initial_verify_state {
                    verify_pipeline.spawn_worker_if_idle();
                }
                if initial_consolidate_state {
                    consolidator_pipeline.spawn_worker_if_idle();
                }
                Ok(Self {
                    storage: storage_arc,
                    keyword: m.clone() as Arc<dyn KeywordPort>,
                    embedder,
                    clock,
                    moon_storage: Some(m),
                    reranker,
                    graph_pipeline,
                    verify_pipeline,
                    consolidator_pipeline,
                })
            }
            "postgres" | "postgresql" => {
                let p = Arc::new(PostgresStorage::connect(url).await?);
                let storage_arc: Arc<dyn StoragePort> = p.clone();
                verify_pipeline.bind_storage(storage_arc.clone());
                verify_pipeline.bind_clock(clock.clone());
                consolidator_pipeline.bind_storage(storage_arc.clone());
                if initial_verify_state {
                    verify_pipeline.spawn_worker_if_idle();
                }
                if initial_consolidate_state {
                    consolidator_pipeline.spawn_worker_if_idle();
                }
                Ok(Self {
                    storage: storage_arc,
                    keyword: p as Arc<dyn KeywordPort>,
                    embedder,
                    clock,
                    moon_storage: None,
                    reranker,
                    graph_pipeline,
                    verify_pipeline,
                    consolidator_pipeline,
                })
            }
            other => Err(LunarisError::Storage(lunaris_core::StorageError::UnsupportedScheme(
                other.to_string(),
            ))),
        }
    }

    /// Legacy test / latency-budget-swap escape hatch. Wires a custom
    /// storage handle, embedder, and clock — bypasses [`Self::open`]'s
    /// default constructors. The keyword Arc is taken from the same
    /// `storage` Arc by attempting an Arc-to-trait downcast — when the
    /// caller's storage type also impls `KeywordPort`, this works
    /// transparently. Otherwise the keyword path returns
    /// `StorageError::NotSupported` at call time.
    ///
    /// Production callers should use [`Self::open`] OR
    /// [`Self::with_parts_keyword`] with explicit `keyword` Arc.
    #[doc(hidden)]
    pub fn with_parts(
        storage: Arc<dyn StoragePort>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<HlcClock>,
    ) -> Self {
        // Plan 04-04 B-10: construct the verify + consolidator pipelines
        // BEFORE the Self struct so we can call bind_storage on each handle
        // with the storage Arc.
        let verify_pipeline = Arc::new(VerifierPipelineHandle::new(
            false,
            Arc::new(NoopVerifier) as Arc<dyn Verifier>,
        ));
        let consolidator_pipeline = Arc::new(ConsolidatorPipelineHandle::new(
            false,
            Arc::new(NoopConsolidator) as Arc<dyn Consolidator>,
        ));
        // B-10: bind storage to BOTH pipelines (2 of the 4 total bind_storage
        // call sites in handle.rs). Also bind the HlcClock to verify_pipeline
        // so the Plan 04-04 Task 4 apply_supersede has a tick source.
        verify_pipeline.bind_storage(storage.clone());
        verify_pipeline.bind_clock(clock.clone());
        consolidator_pipeline.bind_storage(storage.clone());
        Self {
            storage,
            keyword: Arc::new(NoKeywordSupport) as Arc<dyn KeywordPort>,
            embedder,
            clock,
            moon_storage: None,
            // Default to NoopReranker so existing callers (Plan 02-01 smoke
            // tests) keep working without picking up the candle dep
            // transitively. Production callers swap via with_reranker.
            reranker: Arc::new(NoopReranker) as Arc<dyn Reranker>,
            // Plan 03-03: graph pipeline OFF by default with a NoopExtractor
            // installed. Tests that exercise the graph-ON path call
            // `handle.graph_pipeline().enable()` + `handle.with_extractor(...)`
            // explicitly; default-OFF preserves the Phase 2 fast path.
            graph_pipeline: Arc::new(GraphPipelineHandle::new(
                false,
                Arc::new(NoopExtractor) as Arc<dyn Extractor>,
            )),
            verify_pipeline,
            consolidator_pipeline,
        }
    }

    /// Test seam used by Plan 02-02 Task 3's `recall_smoke` — wire a
    /// `KeywordPort` Arc explicitly. Production callers go through
    /// [`Self::open`] which constructs both Arcs from the URL.
    #[doc(hidden)]
    pub fn with_parts_keyword(
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<HlcClock>,
    ) -> Self {
        // Plan 04-04 B-10: same shape as with_parts — construct the pipeline
        // handles BEFORE the Self struct, then bind_storage on both.
        let verify_pipeline = Arc::new(VerifierPipelineHandle::new(
            false,
            Arc::new(NoopVerifier) as Arc<dyn Verifier>,
        ));
        let consolidator_pipeline = Arc::new(ConsolidatorPipelineHandle::new(
            false,
            Arc::new(NoopConsolidator) as Arc<dyn Consolidator>,
        ));
        // B-10: bind storage to BOTH pipelines (the OTHER 2 of the 4 total
        // bind_storage call sites in handle.rs). Also bind the HlcClock to
        // verify_pipeline.
        verify_pipeline.bind_storage(storage.clone());
        verify_pipeline.bind_clock(clock.clone());
        consolidator_pipeline.bind_storage(storage.clone());
        Self {
            storage,
            keyword,
            embedder,
            clock,
            moon_storage: None,
            reranker: Arc::new(NoopReranker) as Arc<dyn Reranker>,
            // Plan 03-03 — see `with_parts` for the rationale.
            graph_pipeline: Arc::new(GraphPipelineHandle::new(
                false,
                Arc::new(NoopExtractor) as Arc<dyn Extractor>,
            )),
            verify_pipeline,
            consolidator_pipeline,
        }
    }

    /// Public escape hatch — replace the embedder on an existing handle.
    /// Used by the Plan 02-01 latency-budget swap (candle → Ollama).
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Plan 02-03 escape hatch — replace the reranker on an existing handle.
    /// Tests pass `Arc::new(NoopReranker)` for determinism; production callers
    /// can wire a custom cross-encoder (e.g., a remote rerank service) without
    /// touching the rest of the construction path. Per RETRIEVE-06 this is
    /// also how callers turn the rerank pass off entirely if the per-batch
    /// budget busts on their hardware: `handle.with_reranker(Arc::new(NoopReranker))`.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Plan 03-03 escape hatch — replace the extractor on an existing handle.
    /// Production callers wiring a `CloudApiExtractor` (cfg-gated behind the
    /// `cloud-api` feature) or a custom [`lunaris_extract::Extractor`] impl
    /// use this; tests pass `Arc::new(lunaris_extract::NoopExtractor)` for
    /// determinism.
    ///
    /// Note: the extractor lives inside the [`GraphPipelineHandle`]'s
    /// `RwLock<Option<Arc<dyn Extractor>>>` — this method swaps it via
    /// [`GraphPipelineHandle::set_extractor`], NOT by replacing the entire
    /// `graph_pipeline` field. Toggle state and the state-change counter are
    /// preserved across the swap (D-12 idempotent observability).
    pub fn with_extractor(self, extractor: Arc<dyn Extractor>) -> Self {
        self.graph_pipeline.set_extractor(extractor);
        self
    }

    /// Plan 04-04 escape hatch — replace the verifier on an existing handle.
    /// Production callers wiring `CandleGemma3_27B` (cfg-gated `candle`) /
    /// `OllamaVerifier` / `CloudApiVerifier` use this; tests pass
    /// `Arc::new(NoopVerifier)` for determinism.
    ///
    /// The verifier lives inside the [`VerifierPipelineHandle`]'s
    /// `RwLock<Option<Arc<dyn Verifier>>>` — this method swaps it via
    /// [`VerifierPipelineHandle::set_verifier`], NOT by replacing the entire
    /// `verify_pipeline` field. Toggle state and the state-change counter are
    /// preserved across the swap (D-12 idempotent observability).
    pub fn with_verifier(self, verifier: Arc<dyn Verifier>) -> Self {
        self.verify_pipeline.set_verifier(verifier);
        self
    }

    /// Plan 04-04 escape hatch — replace the consolidator on an existing handle.
    /// Production callers install a real ACT-R consolidator via this; tests pass
    /// `Arc::new(NoopConsolidator)` for determinism.
    ///
    /// Same swap semantics as [`Self::with_verifier`] — toggle + counter
    /// preserved.
    pub fn with_consolidator(self, consolidator: Arc<dyn Consolidator>) -> Self {
        self.consolidator_pipeline.set_consolidator(consolidator);
        self
    }

    /// Borrow accessors — needed by Plan 02-02's retrieve DSL builder.
    pub fn storage(&self) -> Arc<dyn StoragePort> {
        self.storage.clone()
    }
    pub fn keyword(&self) -> Arc<dyn KeywordPort> {
        self.keyword.clone()
    }
    pub fn embedder(&self) -> Arc<dyn Embedder> {
        self.embedder.clone()
    }
    pub fn clock(&self) -> Arc<HlcClock> {
        self.clock.clone()
    }
    /// Borrow the typed `Arc<MoonStorage>` when the handle was opened against
    /// a Moon backend; `None` otherwise. Plan 02-02's `recall()` plumbs this
    /// into the `RetrievalBuilder` so `fuse_rrf` can opt into Moon-native
    /// hybrid search.
    pub fn moon_storage(&self) -> Option<Arc<MoonStorage>> {
        self.moon_storage.clone()
    }
    /// Borrow the configured reranker. Lets callers chain
    /// `handle.recall().rerank(handle.reranker())` when they want the rerank
    /// pass without re-declaring it.
    pub fn reranker(&self) -> Arc<dyn Reranker> {
        self.reranker.clone()
    }

    /// Plan 03-03 — borrow the [`GraphPipelineHandle`] for runtime toggle
    /// control. EXTRACT-06 single-switch surface (D-10):
    ///
    /// - `handle.graph_pipeline().enable()` / `.disable()` — flip the
    ///   pipeline ON / OFF (idempotent, observable per D-12).
    /// - `handle.graph_pipeline().is_enabled()` — current state.
    /// - `handle.graph_pipeline().force_reload().await` — reload the
    ///   extractor from default cache (e.g., after `huggingface-cli` finished
    ///   downloading weights).
    pub fn graph_pipeline(&self) -> Arc<GraphPipelineHandle> {
        self.graph_pipeline.clone()
    }

    /// Plan 03-03 — snapshot the currently-installed [`Extractor`] `Arc`.
    /// Useful for the canonical compose example in tests + bench harnesses.
    /// Returns `None` only when the [`GraphPipelineHandle`] has no extractor
    /// installed (rare — only via explicit `set_extractor` with a None which
    /// is not exposed in the public surface; the public surface always
    /// installs at least [`NoopExtractor`]).
    pub fn extractor(&self) -> Option<Arc<dyn Extractor>> {
        self.graph_pipeline.snapshot_extractor()
    }

    /// Plan 04-04 — borrow the [`VerifierPipelineHandle`] for runtime toggle
    /// control. D-08 single-switch surface:
    ///
    /// - `handle.verify_pipeline().enable()` / `.disable()` — flip the
    ///   pipeline ON / OFF (idempotent, observable per D-12). Spawns / signals
    ///   shutdown on the in-process tokio worker.
    /// - `handle.verify_pipeline().is_enabled()` — current state.
    /// - `handle.verify_pipeline().join_worker().await` — await full worker
    ///   exit after a `disable()`.
    pub fn verify_pipeline(&self) -> Arc<VerifierPipelineHandle> {
        self.verify_pipeline.clone()
    }

    /// Plan 04-04 — borrow the [`ConsolidatorPipelineHandle`] for runtime
    /// toggle control. Same surface shape as [`Self::verify_pipeline`].
    pub fn consolidator_pipeline(&self) -> Arc<ConsolidatorPipelineHandle> {
        self.consolidator_pipeline.clone()
    }

    /// Plan 04-04 — snapshot the currently-installed [`Verifier`] `Arc`.
    pub fn verifier(&self) -> Option<Arc<dyn Verifier>> {
        self.verify_pipeline.snapshot_verifier()
    }

    /// Plan 04-04 — snapshot the currently-installed [`Consolidator`] `Arc`.
    pub fn consolidator(&self) -> Option<Arc<dyn Consolidator>> {
        self.consolidator_pipeline.snapshot_consolidator()
    }
}

/// Sentinel `KeywordPort` impl returned by [`Lunaris::with_parts`] when the
/// caller did NOT supply a real keyword backend. Calling `keyword_search`
/// returns `StorageError::NotSupported` so callers see a clear failure
/// rather than a silent empty result.
#[derive(Debug, Clone, Copy)]
struct NoKeywordSupport;

#[async_trait::async_trait]
impl KeywordPort for NoKeywordSupport {
    async fn keyword_search(
        &self,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&lunaris_core::Filter>,
        _as_of: Option<lunaris_core::Hlc>,
    ) -> Result<Vec<lunaris_core::KeywordHit>, lunaris_core::StorageError> {
        Err(lunaris_core::StorageError::NotSupported(
            "Lunaris::with_parts was called without a KeywordPort — use with_parts_keyword or open(url)",
        ))
    }
}

/// Construct the default embedder for [`Lunaris::open`]. Uses the candle
/// backend when available, falls back to Ollama otherwise. When neither is
/// compiled in, returns an actionable error so the caller knows to use
/// [`Lunaris::with_parts`].
#[cfg(feature = "candle")]
async fn default_embedder() -> Result<Arc<dyn Embedder>, LunarisError> {
    let e = lunaris_embed::CandleEmbeddingGemma::new(Default::default()).await?;
    Ok(Arc::new(e) as Arc<dyn Embedder>)
}

#[cfg(all(not(feature = "candle"), feature = "ollama"))]
async fn default_embedder() -> Result<Arc<dyn Embedder>, LunarisError> {
    let e = lunaris_embed::OllamaEmbedder::new(Default::default())?;
    Ok(Arc::new(e) as Arc<dyn Embedder>)
}

#[cfg(not(any(feature = "candle", feature = "ollama")))]
async fn default_embedder() -> Result<Arc<dyn Embedder>, LunarisError> {
    Err(LunarisError::Storage(lunaris_core::StorageError::NotSupported(
        "no default embedder compiled in — enable feature `candle` or `ollama`, or pass a custom embedder via Lunaris::with_parts",
    )))
}

/// Plan 02-03: Construct the default reranker for [`Lunaris::open`].
///
/// Tries to load `BgeRerankerV2M3` from the default cache. On cache miss
/// (the common case on dev boxes without 1.1 GB of weights pre-downloaded)
/// emits a `tracing::warn!` describing what's missing and substitutes
/// [`NoopReranker`] per the RETRIEVE-06 contract — recall still runs
/// end-to-end, just without the 12 ms cross-encoder relevance boost.
///
/// Callers wire their own reranker via `Lunaris::with_reranker(reranker)`.
#[cfg(feature = "candle")]
async fn default_reranker() -> Arc<dyn Reranker> {
    match lunaris_rerank::BgeRerankerV2M3::try_new_from_default_cache().await {
        Ok(r) => Arc::new(r) as Arc<dyn Reranker>,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "bge-reranker-v2-m3 unavailable; using NoopReranker (recall budget skips the rerank pass per RETRIEVE-06 contract — install weights via `python -m huggingface_hub download BAAI/bge-reranker-v2-m3 --local-dir ~/.cache/lunaris/models/bge-reranker-v2-m3`)"
            );
            Arc::new(NoopReranker) as Arc<dyn Reranker>
        }
    }
}

/// Without the `candle` feature there's no real reranker to fall back FROM,
/// so the default is unconditionally [`NoopReranker`]. Production callers that
/// want a real rerank pass enable the `candle` feature OR pass a custom
/// `Reranker` impl via [`Lunaris::with_reranker`].
#[cfg(not(feature = "candle"))]
async fn default_reranker() -> Arc<dyn Reranker> {
    Arc::new(NoopReranker) as Arc<dyn Reranker>
}

/// Plan 03-03: Construct the default extractor for [`Lunaris::open`].
///
/// Tries to load [`lunaris_extract::CandleGemma3_4B`] from the default cache.
/// On cache miss (the common case on dev boxes without ~3 GiB of Gemma-3 4B
/// weights pre-downloaded) emits a `tracing::warn!` describing what's missing
/// AND substitutes [`NoopExtractor`] per the D-11 dead-code-when-OFF
/// contract — even when the feature is enabled, a missing cache produces a
/// working binary that has the trait wired but never calls a real model.
/// Mirrors the [`default_reranker`] BgeRerankerV2M3 pattern from Plan 02-03.
///
/// Callers wire their own extractor via [`Lunaris::with_extractor`] or
/// `handle.graph_pipeline().set_extractor(extractor)` for late binding.
#[cfg(feature = "candle")]
async fn default_extractor() -> Arc<dyn Extractor> {
    match lunaris_extract::CandleGemma3_4B::new(Default::default()).await {
        Ok(e) => Arc::new(e) as Arc<dyn Extractor>,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "gemma-3-4b-it unavailable; using NoopExtractor (graph extraction disabled at runtime — install weights via `huggingface-cli download google/gemma-3-4b-it --local-dir ~/.cache/lunaris/models/gemma-3-4b-it`)"
            );
            Arc::new(NoopExtractor) as Arc<dyn Extractor>
        }
    }
}

/// Without `candle` there's no real extractor to fall back FROM. Production
/// callers wanting graph extraction either enable the `candle` feature or
/// pass a custom [`Extractor`] impl (e.g., [`lunaris_extract::OllamaExtractor`]
/// under the `ollama` feature) via [`Lunaris::with_extractor`].
#[cfg(not(feature = "candle"))]
async fn default_extractor() -> Arc<dyn Extractor> {
    Arc::new(NoopExtractor) as Arc<dyn Extractor>
}

/// Plan 04-04: Construct the default verifier for [`Lunaris::open`].
///
/// Tries to load [`lunaris_verify::CandleGemma3_27B`] from the default cache.
/// On cache miss (the common case on dev boxes without ~14 GiB of Gemma-3 27B
/// weights pre-downloaded) emits a `tracing::warn!` and substitutes
/// [`NoopVerifier`] per the D-02 default-OFF contract.
///
/// Mirrors the [`default_extractor`] CandleGemma3_4B pattern — even when the
/// `candle` feature is enabled, a missing cache produces a working binary
/// that has the trait wired but never calls a real model.
///
/// Callers wire their own verifier via [`Lunaris::with_verifier`].
#[cfg(feature = "candle")]
async fn default_verifier() -> Arc<dyn Verifier> {
    match lunaris_verify::CandleGemma3_27B::new(Default::default()).await {
        Ok(v) => Arc::new(v) as Arc<dyn Verifier>,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "gemma-3-27b-it unavailable; using NoopVerifier (verifier worker disabled at runtime — install weights via `huggingface-cli download google/gemma-3-27b-it --local-dir ~/.cache/lunaris/models/gemma-3-27b-it`)"
            );
            Arc::new(NoopVerifier) as Arc<dyn Verifier>
        }
    }
}

/// Without `candle` there's no real verifier to fall back FROM. Production
/// callers wanting verifier work either enable the `candle` feature or pass
/// a custom [`Verifier`] impl via [`Lunaris::with_verifier`].
#[cfg(not(feature = "candle"))]
async fn default_verifier() -> Arc<dyn Verifier> {
    Arc::new(NoopVerifier) as Arc<dyn Verifier>
}

/// Plan 04-04: Construct the default consolidator for [`Lunaris::open`].
///
/// Per D-15 + lunaris-consolidate's no-LLM-backends posture, the v0 default
/// is unconditionally [`NoopConsolidator`]. Real ACT-R consolidation is
/// deferred to v1 (CONSOL-V1-01) — production callers wire it explicitly via
/// [`Lunaris::with_consolidator`] when they're ready to flip the pipeline ON.
fn default_consolidator() -> Arc<dyn Consolidator> {
    Arc::new(NoopConsolidator) as Arc<dyn Consolidator>
}
