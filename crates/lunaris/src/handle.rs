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

use std::sync::{Arc, OnceLock};

use lunaris_consolidate::Consolidator;
use lunaris_core::{
    Embedder, HlcClock, KeywordPort, Lsn, LunarisError, Scope, StorageError, StoragePort,
};
use ulid::Ulid;

use crate::episode_builder::EpisodeBuilder;
use lunaris_extract::{Extractor, NoopExtractor};
use lunaris_rerank::{NoopReranker, Reranker};
use lunaris_storage_embedded::EmbeddedStorage;
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_postgres::PostgresStorage;
use lunaris_verify::{
    BOOST_DELTA, NoopReflectSupervisor, NoopVerifier, ReflectInput, ReflectOutput,
    ReflectSupervisor, Verifier, apply_reflect_boost, apply_reflect_invalidate,
    boost_cache_capacity,
};

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
    /// Phase 13 — per-turn reflection supervisor. Default OFF
    /// (`NoopReflectSupervisor`), matching blueprint §5.1 default-OFF pattern
    /// for all optional LLM pipeline stages. Callers install a real supervisor
    /// via [`Self::with_reflect_supervisor`] and call [`Self::end_turn`] at the
    /// end of each agent turn to trigger the reflection pass.
    pub(crate) reflect_supervisor: Arc<dyn ReflectSupervisor>,
    /// Phase 14.2 — ephemeral per-handle LRU boost cache.
    ///
    /// Populated by [`ScopedLunaris::end_turn`] from
    /// [`lunaris_verify::ReflectOutput::boost`]; consumed as a post-hydrate
    /// rescorer by every [`lunaris_retrieve::RetrievalBuilder`] returned from
    /// [`Self::recall`]. Cache key is `(Scope, Ulid)` so boost signals from
    /// one tenant scope never leak into another scope's recall results.
    ///
    /// Lock discipline: the guard is acquired, all entries are written /
    /// read, then the guard is dropped before the next `.await` point. This
    /// upholds the CLAUDE.md "never hold a lock across `.await`" invariant.
    ///
    /// Capacity: controlled by `LUNARIS_BOOST_CACHE_CAPACITY` (default 10 000)
    /// via [`lunaris_verify::boost_cache_capacity`].
    pub(crate) boost_cache: Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>>,
    /// Phase 14.3 — concurrency bound for speculative warm-up recalls spawned
    /// by [`ScopedLunaris::end_turn`] when [`ReflectOutput::pre_warm_query`] is
    /// `Some`. Capacity defaults to 4; override via
    /// `LUNARIS_PREWARM_CONCURRENCY` env var (positive integer; 0 or
    /// non-numeric values fall back to the default). If the semaphore is
    /// exhausted when `end_turn` fires, the warm-up is silently skipped (logged
    /// at `DEBUG`) — `end_turn` never blocks on the semaphore.
    pub(crate) warm_up_semaphore: Arc<tokio::sync::Semaphore>,
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
            .field("consolidator_pipeline_enabled", &self.consolidator_pipeline.is_enabled())
            .field("reflect_supervisor_applies", &self.reflect_supervisor.applies())
            .field("boost_cache_len", &self.boost_cache.read().len())
            .field("warm_up_semaphore_permits", &self.warm_up_semaphore.available_permits())
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
    ///   to client-side reciprocal rank fusion. If `LUNARIS_ADMIN_URL` is set,
    ///   migrations run over that DDL-capable admin connection first and the
    ///   handle binds to (the possibly non-DDL) `url` — no out-of-band
    ///   `sqlx migrate run`.
    /// - `memory://` / `sqlite:///path` →
    ///   [`lunaris_storage_embedded::EmbeddedStorage`] — the zero-dependency
    ///   dev/embedded backend (no Docker, no Postgres, no Moon). `memory://`
    ///   is in-process and ephemeral; `sqlite:///path` is file-backed.
    ///
    /// ## Default backend resolution (Phase 20-02)
    ///
    /// Embedder and reranker are resolved from environment variables:
    ///
    /// - `LUNARIS_EMBEDDER_BACKEND` ∈ `{fastembed, candle, ollama}` —
    ///   default **`fastembed`** (auto-downloads ONNX EmbeddingGemma 300M
    ///   weights to `~/.cache/lunaris/models/fastembed/` on first call).
    /// - `LUNARIS_RERANKER_BACKEND` ∈ `{fastembed, candle, noop}` —
    ///   default **`fastembed`** (auto-downloads ONNX BGE-Reranker-V2-M3
    ///   on first call).
    /// - `LUNARIS_VERIFIER_BACKEND` ∈ `{270m, small, 27b, large, noop}` —
    ///   default **`270m`** (RFC 0006 laptop-floor verifier). `27b/large`
    ///   requires `--features verify-large`; `270m/small` requires
    ///   `--features verify-small`. Cache miss / feature off → tracing
    ///   warn + `NoopVerifier` (verifier worker disabled at runtime).
    /// - `LUNARIS_CONSOLIDATOR_BACKEND` ∈ `{actr, noop}` — default
    ///   **`actr`** (production ACT-R consolidator). Fail-fast on
    ///   unknown values.
    ///
    /// Unknown env values fail fast with `LunarisError::Storage(Backend(...))`
    /// — there is **no** silent fallback. Empty string / unset both use the
    /// default. See `resolve_embedder`, `resolve_reranker`,
    /// `default_verifier`, and `default_consolidator` (private helpers).
    ///
    /// Air-gapped deployments: set the env vars to `candle` and pre-stage
    /// weights via `huggingface-cli`; OR build with
    /// `--no-default-features --features candle-only` to strip fastembed/ort/hf-hub
    /// from the dep tree entirely. See
    /// `docs/migration/0.1-to-0.2-fastembed-default.md`.
    pub async fn open(url: &str) -> Result<Self, LunarisError> {
        let embedder = resolve_embedder().await?;
        Self::open_with_embedder(url, embedder).await
    }

    /// Like [`Lunaris::open`] but uses the caller-provided `embedder`
    /// directly instead of resolving one from `LUNARIS_EMBEDDER_BACKEND`.
    ///
    /// Use this when:
    ///
    /// - The compile-time feature set has no real embedder backend and the
    ///   silent-fallback `NoopEmbedder` is unacceptable — pass a BYO
    ///   embedder you constructed elsewhere.
    /// - You need to pin a specific vector dim BEFORE Moon creates its FT
    ///   indices. Moon's `FT.CREATE` is idempotent and DOES NOT auto-resize
    ///   an existing index, so post-`open()` `with_embedder` calls cannot
    ///   change the on-disk dim of an existing collection. This method runs
    ///   the embedder's `dim()` through `MoonStorage::connect_with_dim` on
    ///   first open, which is the right time to size the index.
    /// - You want a `NoopEmbedder` at a specific dim:
    ///   ```ignore
    ///   use std::sync::Arc;
    ///   use lunaris::Lunaris;
    ///   use lunaris_embed::NoopEmbedder;
    ///   let handle = Lunaris::open_with_embedder(
    ///       "moon://localhost:6390",
    ///       Arc::new(NoopEmbedder::new(1536)),
    ///   ).await?;
    ///   ```
    ///
    /// The reranker / extractor / verifier / consolidator are still
    /// resolved from their env vars exactly as in [`Lunaris::open`].
    pub async fn open_with_embedder(
        url: &str,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self, LunarisError> {
        let scheme = url.split("://").next().unwrap_or("");
        let clock = HlcClock::new(0);
        let reranker = resolve_reranker().await?;
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
        // Phase 16-01 (CONSOL-V1-01): resolve backend from LUNARIS_CONSOLIDATOR_BACKEND;
        // fail-fast on unknown env values (no silent fallback).
        let consolidator = default_consolidator()?;
        let initial_verify_state = VerifierPipelineHandle::initial_state_from_env();
        let initial_consolidate_state = ConsolidatorPipelineHandle::initial_state_from_env();
        let verify_pipeline = Arc::new(VerifierPipelineHandle::new(initial_verify_state, verifier));
        let consolidator_pipeline =
            Arc::new(ConsolidatorPipelineHandle::new(initial_consolidate_state, consolidator));
        match scheme {
            "moon" => {
                // Size the Moon FT vector indices to the resolved embedder's
                // dimension (default 768-d for EmbeddingGemma; pass a wider
                // embedder via LUNARIS_EMBEDDER_BACKEND and the indices grow to
                // match). Moon's FT.CREATE has no dimension cap. Footgun: if the
                // Moon instance already holds indices at a different dim, they
                // are NOT auto-resized — drop them first.
                let m = Arc::new(MoonStorage::connect_with_dim(url, embedder.dim()).await?);
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
                    reflect_supervisor: Arc::new(NoopReflectSupervisor),
                    boost_cache: Arc::new(parking_lot::RwLock::new(
                        lru::LruCache::new(boost_cache_capacity()),
                    )),
                    warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                        resolve_prewarm_concurrency(),
                    )),
                })
            }
            "postgres" | "postgresql" => {
                // OPS — onboarding: if `LUNARIS_ADMIN_URL` is set, migrate over
                // that DDL-capable admin connection first, then bind the handle
                // to (the possibly non-DDL) `url` without re-running migrations.
                // Unset → legacy behaviour: migrate as the role behind `url`,
                // with an actionable hint if it lacks DDL and the schema is
                // behind. This removes the out-of-band `sqlx migrate run` step.
                let admin_url =
                    std::env::var("LUNARIS_ADMIN_URL").ok().filter(|s| !s.trim().is_empty());
                let p =
                    Arc::new(PostgresStorage::connect_with_admin(url, admin_url.as_deref()).await?);
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
                    reflect_supervisor: Arc::new(NoopReflectSupervisor),
                    boost_cache: Arc::new(parking_lot::RwLock::new(
                        lru::LruCache::new(boost_cache_capacity()),
                    )),
                    warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                        resolve_prewarm_concurrency(),
                    )),
                })
            }
            // Onboarding overhaul (phase 1): the zero-dependency embedded
            // backend — `memory://` (in-process) / `sqlite:///path` (file).
            // Same wiring shape as the Postgres arm; `EmbeddedStorage` impls
            // both `StoragePort` and `KeywordPort`.
            "memory" | "sqlite" => {
                let e = Arc::new(EmbeddedStorage::connect(url).await?);
                let storage_arc: Arc<dyn StoragePort> = e.clone();
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
                    keyword: e as Arc<dyn KeywordPort>,
                    embedder,
                    clock,
                    moon_storage: None,
                    reranker,
                    graph_pipeline,
                    verify_pipeline,
                    consolidator_pipeline,
                    reflect_supervisor: Arc::new(NoopReflectSupervisor),
                    boost_cache: Arc::new(parking_lot::RwLock::new(
                        lru::LruCache::new(boost_cache_capacity()),
                    )),
                    warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                        resolve_prewarm_concurrency(),
                    )),
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
        // Phase 16-01 (CONSOL-V1-01): resolve backend from env. Test seam is
        // infallible — `expect` surfaces env misconfiguration loudly rather
        // than silently falling back (matches fail-fast contract of the
        // `Lunaris::open` path).
        let consolidator = ConsolidatorPipelineHandle::backend_from_env()
            .expect("LUNARIS_CONSOLIDATOR_BACKEND resolution failed in with_parts test seam");
        let consolidator_pipeline = Arc::new(ConsolidatorPipelineHandle::new(false, consolidator));
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
            // Phase 13 — default OFF per blueprint §5.1 default-OFF pattern.
            reflect_supervisor: Arc::new(NoopReflectSupervisor),
            // Phase 14.2 — ephemeral boost cache, capacity from env (default 10_000).
            boost_cache: Arc::new(parking_lot::RwLock::new(
                lru::LruCache::new(boost_cache_capacity()),
            )),
            // Phase 14.3 — semaphore for bounded fire-and-forget warm-up spawns.
            warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                resolve_prewarm_concurrency(),
            )),
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
        // Phase 16-01 (CONSOL-V1-01): resolve backend from env (same fail-fast
        // contract as `with_parts`).
        let consolidator = ConsolidatorPipelineHandle::backend_from_env().expect(
            "LUNARIS_CONSOLIDATOR_BACKEND resolution failed in with_parts_keyword test seam",
        );
        let consolidator_pipeline = Arc::new(ConsolidatorPipelineHandle::new(false, consolidator));
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
            // Phase 13 — default OFF per blueprint §5.1 default-OFF pattern.
            reflect_supervisor: Arc::new(NoopReflectSupervisor),
            // Phase 14.2 — ephemeral boost cache, capacity from env (default 10_000).
            boost_cache: Arc::new(parking_lot::RwLock::new(
                lru::LruCache::new(boost_cache_capacity()),
            )),
            // Phase 14.3 — semaphore for bounded fire-and-forget warm-up spawns.
            warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                resolve_prewarm_concurrency(),
            )),
        }
    }

    /// Public escape hatch — replace the embedder on an existing handle.
    ///
    /// [`Lunaris::open`] resolves the default embedder from
    /// [`EMBEDDER_BACKEND_ENV_VAR`] (`LUNARIS_EMBEDDER_BACKEND`), defaulting
    /// to `fastembed` since Phase 20-02 (2026-05-11). Override via the env var
    /// at startup OR call this method post-construction to swap in any
    /// `Arc<dyn Embedder>` (e.g., a `StubEmbedder` in tests, or a remote
    /// embedder service). Used by the Plan 02-01 latency-budget swap
    /// (candle → Ollama).
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Plan 02-03 escape hatch — replace the reranker on an existing handle.
    ///
    /// [`Lunaris::open`] resolves the default reranker from
    /// [`RERANKER_BACKEND_ENV_VAR`] (`LUNARIS_RERANKER_BACKEND`), defaulting
    /// to `fastembed` since Phase 20-02. Tests pass `Arc::new(NoopReranker)`
    /// for determinism; production callers can wire a custom cross-encoder
    /// (e.g., a remote rerank service) without touching the rest of the
    /// construction path. Per RETRIEVE-06 this is also how callers turn the
    /// rerank pass off entirely if the per-batch budget busts on their
    /// hardware: `handle.with_reranker(Arc::new(NoopReranker))` (or set
    /// `LUNARIS_RERANKER_BACKEND=noop` at startup).
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

    /// Phase 13 escape hatch — replace the reflection supervisor on an existing
    /// handle. Production callers install an [`LlmReflectSupervisor`] (or a
    /// custom [`ReflectSupervisor`] impl) via this; tests pass
    /// `Arc::new(NoopReflectSupervisor)` for determinism.
    ///
    /// Unlike `verify_pipeline` and `consolidator_pipeline`, the reflect
    /// supervisor is a plain `Arc` (no background worker, no toggle) — it is
    /// invoked synchronously per [`Self::end_turn`] call on the caller's task.
    ///
    /// [`LlmReflectSupervisor`]: lunaris_verify::LlmReflectSupervisor
    pub fn with_reflect_supervisor(mut self, supervisor: Arc<dyn ReflectSupervisor>) -> Self {
        self.reflect_supervisor = supervisor;
        self
    }

    /// Phase 13 — signal the end of an agent turn and run the reflection pass.
    ///
    /// Calls [`ReflectSupervisor::reflect`] with `input` and returns the
    /// advisory [`ReflectOutput`] (`invalidate`, `boost`, `pre_warm_query`).
    ///
    /// ## Budget + failure discipline
    ///
    /// The supervisor enforces its own timeout (default 500 ms for
    /// [`LlmReflectSupervisor`]). If the supervisor returns `Err`, this method
    /// propagates it — callers that treat reflect as best-effort should wrap
    /// with `.unwrap_or_default()`. If the installed supervisor is
    /// [`NoopReflectSupervisor`] (the default), this call is a cheap no-op
    /// returning `ReflectOutput::default()`.
    ///
    /// ## Non-requirements in this commit
    ///
    /// The returned [`ReflectOutput`] is **advisory only** — storage-side
    /// application (`invalidate` → `BiTemporal::invalidate_sys`, `boost` →
    /// retrieval-rank adjustment, `pre_warm_query` → speculative recall) is a
    /// Phase 13 follow-up. For now, the output is logged and returned to the
    /// caller.
    ///
    /// [`LlmReflectSupervisor`]: lunaris_verify::LlmReflectSupervisor
    pub async fn end_turn(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
        let turn_id = input.turn_id;
        let output = self.reflect_supervisor.reflect(input).await?;
        tracing::info!(
            target: "lunaris::handle",
            turn_id = ?turn_id,
            invalidate_count = output.invalidate.len(),
            boost_count = output.boost.len(),
            pre_warm_query = output.pre_warm_query.is_some(),
            "end_turn_reflect_complete"
        );
        Ok(output)
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

    /// Phase 13 — borrow the configured [`ReflectSupervisor`] `Arc`.
    /// Returns the currently-installed supervisor — `NoopReflectSupervisor` by
    /// default, or whatever was last passed to [`Self::with_reflect_supervisor`].
    pub fn reflect_supervisor(&self) -> Arc<dyn ReflectSupervisor> {
        self.reflect_supervisor.clone()
    }

    /// Phase 14.3 — borrow the warm-up semaphore `Arc`.
    ///
    /// Primarily for testing: callers can assert `available_permits()` to
    /// verify the semaphore was / was not acquired.
    pub fn warm_up_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        self.warm_up_semaphore.clone()
    }

    /// Phase 14.3 test seam — replace the warm-up semaphore with a custom
    /// capacity. Use in integration tests that need to control the concurrency
    /// bound (e.g., capacity=1 for the semaphore-bound test).
    ///
    /// This is intentionally `#[doc(hidden)]` — production code uses the
    /// env-var knob (`LUNARIS_PREWARM_CONCURRENCY`) at construction time.
    #[doc(hidden)]
    pub fn with_prewarm_concurrency(mut self, capacity: usize) -> Self {
        self.warm_up_semaphore = Arc::new(tokio::sync::Semaphore::new(capacity));
        self
    }

    /// RFC 0001 Wave 0 — construct a scope-bound view over this handle.
    ///
    /// All operations issued through the returned [`ScopedLunaris`] carry
    /// `scope` as their partitioning key. The underlying `Lunaris` handle is
    /// borrowed for the lifetime `'a` — no cloning occurs.
    ///
    /// Wave 1 will route each method through the real scope-aware backends.
    /// Wave 0 stubs return `todo!()` so the API surface is frozen before the
    /// routing logic lands.
    pub fn scoped(&self, scope: Scope) -> ScopedLunaris<'_> {
        ScopedLunaris { engine: self, scope }
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
    /// Wave 2.5A: gains `scope: &Scope` per RFC 0001 §3.4 amendment.
    /// Scope is ignored — this sentinel returns NotSupported regardless.
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
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

/// RFC 0001 Wave 1D — scope-bound view over a [`Lunaris`] handle.
///
/// Constructed via [`Lunaris::scoped`]. All operations issued through this
/// wrapper carry the bound [`Scope`] as their partitioning key. The `'a`
/// lifetime ties the view to the underlying handle so no `Arc` clone is
/// required for the wrapper itself.
///
/// ## Scope enforcement
///
/// Callers build an [`EpisodeBuilder`] (scope-less payload) and pass it to
/// [`Self::ingest`]. The wrapper is the ONLY code path that can call
/// `EpisodeBuilder::into_episode` (it's `pub` but the scope value comes
/// exclusively from this wrapper's `self.scope` field). Callers cannot
/// construct an `Episode` with an arbitrary scope by bypassing this type.
pub struct ScopedLunaris<'a> {
    pub(crate) engine: &'a Lunaris,
    pub(crate) scope: Scope,
}

impl<'a> ScopedLunaris<'a> {
    /// Returns the [`Scope`] this view is bound to.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Ingest an episode payload under the bound scope.
    ///
    /// Takes an [`EpisodeBuilder`] (scope-less payload) rather than a fully
    /// constructed `Episode` so the caller cannot inject an arbitrary scope.
    /// The wrapper stamps `self.scope` onto the episode via
    /// `builder.into_episode(self.scope.clone(), &self.engine.clock)` before
    /// delegating to [`Lunaris::ingest`].
    ///
    /// INGEST-04 invariant: exactly one `atomic_write` call per ingest path.
    /// The write lives in `lunaris_ingest::ingest_episode` (graph OFF) or
    /// `ingest_episode_graph_on` (graph ON), unchanged from the non-scoped path.
    pub async fn ingest(&self, builder: EpisodeBuilder) -> Result<Lsn, LunarisError> {
        let episode = builder.into_episode(self.scope.clone(), &self.engine.clock);
        self.engine.ingest(episode).await
    }

    /// Recall hits under the bound scope.
    ///
    /// Returns a [`lunaris_retrieve::RetrievalBuilder`] pre-seeded with the
    /// engine's storage / embedder / keyword Arcs AND this wrapper's scope,
    /// ready for the caller to customise and `.execute()`. Wave 2.5C: the
    /// scope is now threaded through the entire retrieval path — Vector,
    /// Graph, Keyword operators, and hydrate all use `self.scope` so only
    /// hits from this scope's partition are returned.
    pub async fn recall(
        &self,
        query: lunaris_retrieve::Query,
    ) -> Result<Vec<lunaris_retrieve::Hit>, LunarisError> {
        self.engine.recall().with_scope(self.scope.clone()).execute(query).await
    }

    /// Forget primitive bound to the wrapper's scope. **P0 #1 Wave 2:** this
    /// is the canonical entry point superseding the deprecated
    /// [`Lunaris::forget`]. Today it delegates to the underlying handle's
    /// implementation — the per-scope storage routing (so the call honours
    /// `self.scope` instead of `Scope::dev()`) is tracked as a v0.3 debt
    /// item in `docs/v0.3-known-debt.md`. The deprecation + canonical API
    /// surface lands here so external adopters can migrate today; the
    /// internal routing flips under the hood when the Wave 1D forget
    /// pipeline ships.
    pub async fn forget(
        &self,
        request: impl Into<crate::forget::ForgetRequest>,
    ) -> Result<crate::forget::ForgetReceipt, LunarisError> {
        // scope-dev-allowed: scoped-forget-shim — delegates to the
        // deprecated bare path until the Wave 1D scope-aware forget
        // pipeline replaces the body (tracked in docs/v0.3-known-debt.md).
        #[allow(deprecated)]
        self.engine.forget(request).await
    }

    /// Return a [`lunaris_retrieve::RetrievalBuilder`] bound to the engine's
    /// storage / embedder / keyword Arcs AND this wrapper's scope for
    /// DSL-style query composition.
    ///
    /// ```ignore
    /// let hits = engine.scoped(scope)
    ///     .dsl()
    ///     .with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))
    ///     .execute(Query::text("brown fox"))
    ///     .await?;
    /// ```
    pub fn dsl(&self) -> lunaris_retrieve::RetrievalBuilder {
        // Wave 2.5C: pre-seed scope so all operators in the tree use the
        // bound scope rather than Scope::dev() placeholders.
        self.engine.recall().with_scope(self.scope.clone())
    }

    /// Phase 14.1 — signal the end of an agent turn, run the reflection pass,
    /// and apply MVCC invalidations for every ulid in [`ReflectOutput::invalidate`].
    ///
    /// ## What this does (Phase 14.1)
    ///
    /// 1. Delegates to the handle's [`ReflectSupervisor`] (same as
    ///    [`Lunaris::end_turn`]). If the supervisor is a
    ///    [`NoopReflectSupervisor`] (the default), the reflect call is a
    ///    cheap no-op returning `ReflectOutput::default()`.
    ///
    /// 2. For every ulid in `output.invalidate`, calls
    ///    [`apply_reflect_invalidate`] which:
    ///    - reads the fact row,
    ///    - stamps `bt.sys.1 = Some(now)` (JSON-patched into the payload),
    ///    - commits **one `atomic_write`** for the entire batch (D-11), and
    ///    - publishes one `AuditEvent::ReflectInvalidation` per stamped ulid
    ///      (D-22, fire-and-forget).
    ///
    /// ## What is NOT done in this commit (Phase 14.2 / 14.3)
    ///
    /// - `boost` — deferred to Phase 14.2 (ephemeral LRU per-handle cache).
    /// - `pre_warm_query` — deferred to Phase 14.3 (fire-and-forget recall).
    ///
    /// ## Failure discipline
    ///
    /// Reflect is advisory.  Supervisor errors and storage errors during
    /// invalidation are logged via `tracing::warn!` and swallowed — this
    /// method **never** fails the agent's next turn due to a reflect error.
    /// The full `ReflectOutput` (including `boost` and `pre_warm_query`) is
    /// returned to the caller regardless.
    ///
    /// ## Scope enforcement
    ///
    /// `self.scope` (the JWT-bound partition key) is the sole source of
    /// truth for the storage partition.  The caller cannot inject a different
    /// scope — that is the whole point of `ScopedLunaris`.
    pub async fn end_turn(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
        let turn_id = input.turn_id;

        // Step 1: run the reflection pass (best-effort — never fail the turn).
        let output = match self.engine.reflect_supervisor.reflect(input).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    target: "lunaris::scoped",
                    err = %e,
                    turn_id = ?turn_id,
                    "reflect_supervisor_error; emitting empty output"
                );
                ReflectOutput::default()
            }
        };

        // Step 2 (Phase 14.1): apply invalidations — one atomic_write for the batch.
        if !output.invalidate.is_empty() {
            match apply_reflect_invalidate(
                &self.engine.storage,
                &self.scope,
                &self.engine.clock,
                turn_id,
                &output.invalidate,
            )
            .await
            {
                Ok(stamped) => {
                    tracing::debug!(
                        target: "lunaris::scoped",
                        turn_id = ?turn_id,
                        invalidated_count = stamped.len(),
                        "reflect_invalidate_applied"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "lunaris::scoped",
                        err = %e,
                        turn_id = ?turn_id,
                        "reflect_invalidate_storage_error; continuing"
                    );
                }
            }
        }

        // Step 3 (Phase 14.2): populate the per-handle boost cache for every
        // chunk ulid nominated by the reflect supervisor.
        //
        // `apply_reflect_boost` is synchronous — it acquires the write lock,
        // writes all entries, and drops the guard before returning.  No `.await`
        // appears between the guard acquisition and its release, satisfying the
        // CLAUDE.md lock-across-await invariant.
        if !output.boost.is_empty() {
            apply_reflect_boost(
                &self.engine.boost_cache,
                &self.scope,
                &output.boost,
                BOOST_DELTA,
            );
            tracing::debug!(
                target: "lunaris::scoped",
                turn_id = ?turn_id,
                boost_count = output.boost.len(),
                boost_delta = BOOST_DELTA,
                "reflect_boost_cache_populated"
            );
        }

        // Summary log at turn boundary (Phase 14.1 + 14.2 combined).
        // Step 3 (Phase 14.3): fire-and-forget speculative warm-up recall.
        //
        // If the reflector predicted a next-turn query, spawn a background task
        // to issue a real recall against the storage backend. This populates
        // Moon's FT page cache (or the OS page cache for Postgres) before the
        // agent issues the actual query, reducing first-hit latency on the next
        // turn.
        //
        // Design constraints (§4 of docs/design/phase-14-reflect-output-application.md):
        // - MUST NOT block `end_turn` — use `try_acquire_owned`, never
        //   `acquire_owned().await`.
        // - Concurrency is bounded by `engine.warm_up_semaphore` (default 4,
        //   configurable via `LUNARIS_PREWARM_CONCURRENCY`). Exhausted semaphore
        //   → skip + DEBUG log, never block.
        // - `OwnedSemaphorePermit` moves into the spawned task via
        //   `let _permit = permit;` INSIDE the `async move {}` block so it is
        //   released when the task ends, not when `end_turn` returns.
        // - Errors inside the task become `tracing::warn!` — never propagate,
        //   never panic.
        // - Warm-up uses the same `Scope` as this `ScopedLunaris` handle so no
        //   cross-tenant data can be accessed.
        if let Some(query_str) = output.pre_warm_query.clone() {
            match self.engine.warm_up_semaphore.clone().try_acquire_owned() {
                Ok(permit) => {
                    // Clone all Arcs needed by the spawned task before the move.
                    // `moon_storage` is included so the warm-up uses the Moon-native
                    // one-round-trip FT path when available — without it the task
                    // would take the generic retrieval path and miss the FT cache.
                    let storage = self.engine.storage.clone();
                    let keyword = self.engine.keyword.clone();
                    let embedder = self.engine.embedder.clone();
                    let moon_storage = self.engine.moon_storage.clone();
                    let scope = self.scope.clone();
                    let q = query_str.clone();
                    tokio::spawn(async move {
                        // PERMIT MOVE: `_permit` is dropped when this task ends,
                        // releasing the semaphore slot. It MUST live inside this
                        // `async move {}` block — placing it outside would release
                        // the permit when `end_turn` returns, defeating the bound.
                        let _permit = permit;
                        // Build a default Vector top-30 recall — the goal is to
                        // warm the backend's FT/page cache, not to return results
                        // to the caller. The default root is the same shape used
                        // by `Lunaris::recall()` and `ScopedLunaris::dsl()`.
                        let mut builder = lunaris_retrieve::RetrievalBuilder::from_handle(
                            storage, keyword, embedder,
                        )
                        .with_scope(scope);
                        if let Some(moon) = moon_storage {
                            builder = builder.with_moon_storage(moon);
                        }
                        match builder.execute(lunaris_retrieve::Query::text(q.as_str())).await {
                            Ok(hits) => tracing::debug!(
                                target: "lunaris::scoped",
                                hits = hits.len(),
                                query = %q,
                                "pre_warm_complete"
                            ),
                            Err(e) => tracing::warn!(
                                target: "lunaris::scoped",
                                err = %e,
                                query = %q,
                                "pre_warm_failed"
                            ),
                        }
                    });
                    tracing::debug!(
                        target: "lunaris::scoped",
                        query = %query_str,
                        "pre_warm_spawned"
                    );
                }
                Err(_) => {
                    tracing::debug!(
                        target: "lunaris::scoped",
                        query = %query_str,
                        "pre_warm_skipped_semaphore_full"
                    );
                }
            }
        }

        // Summary log at turn boundary (Phase 14.1 requirement).
        tracing::info!(
            target: "lunaris::scoped",
            turn_id = ?turn_id,
            invalidated_count = output.invalidate.len(),
            boost_count = output.boost.len(),
            pre_warm_query = output.pre_warm_query.is_some(),
            "scoped_end_turn_complete"
        );

        Ok(output)
    }
}

// ── Phase 20-02: env-resolved embedder + reranker backends ──────────────────
//
// `resolve_embedder()` and `resolve_reranker()` mirror the
// `ConsolidatorPipelineHandle::backend_from_env` pattern (see
// crates/lunaris/src/consolidator_pipeline.rs): `.trim()` +
// `eq_ignore_ascii_case`, empty-string treated as unset, unknown values fail
// fast via `LunarisError::Storage(StorageError::Backend(...))` with an
// `"is not one of"` substring for grep-ability. One-shot `tracing::info!`
// per-process on resolution (T-20-02-03 mitigation — operators see which
// backend their deployment is running without per-handle log spam).

/// Env var that pins the embedder backend resolved by [`Lunaris::open`].
/// Domain: `{fastembed, candle, ollama, noop}`. Default (unset / empty)
/// depends on the compile-time feature set:
///
/// - `fastembed` feature on → `fastembed` (preserves Phase 20-02 default).
/// - else `candle` feature on → `candle`.
/// - else `ollama` feature on → `ollama`.
/// - else no embedder feature compiled in → `noop` (Phase 22 — emits a
///   one-shot `tracing::warn!` on resolve so the operator sees that vector
///   recall is dead until they enable a backend or call
///   [`Lunaris::open_with_embedder`]).
pub const EMBEDDER_BACKEND_ENV_VAR: &str = "LUNARIS_EMBEDDER_BACKEND";

/// Env var that controls the dim of the `noop` embedder when it is the
/// resolved backend. Positive integer; default
/// [`lunaris_embed::NOOP_DEFAULT_DIM`] (768). Non-numeric / `0` / unset all
/// fall back to the default with a `tracing::warn!`. Ignored for the
/// real embedder backends (their dim is fixed by the model).
pub const EMBED_DIM_ENV_VAR: &str = "LUNARIS_EMBED_DIM";

/// Env var that pins the reranker backend resolved by [`Lunaris::open`].
/// Domain: `{fastembed, candle, noop}`. Default (unset / empty): `fastembed`.
pub const RERANKER_BACKEND_ENV_VAR: &str = "LUNARIS_RERANKER_BACKEND";

/// Env var that controls the maximum number of concurrent speculative warm-up
/// recall tasks spawned by [`ScopedLunaris::end_turn`] (Phase 14.3).
/// Must be a positive integer. `0`, non-numeric, or unset values fall back to
/// the default of `4`. One `tracing::info!` is emitted per process when the
/// capacity is resolved.
pub const PREWARM_CONCURRENCY_ENV_VAR: &str = "LUNARIS_PREWARM_CONCURRENCY";

/// Default semaphore capacity for speculative warm-up recalls.
const PREWARM_CONCURRENCY_DEFAULT: usize = 4;

/// Resolve the warm-up semaphore capacity from [`PREWARM_CONCURRENCY_ENV_VAR`].
///
/// Non-numeric, `0`, and unset values all return `PREWARM_CONCURRENCY_DEFAULT`
/// with a `tracing::warn!` for non-numeric/zero inputs. Negative values are
/// impossible since we parse as `usize`. A one-shot `tracing::info!` is emitted
/// per process on the resolved capacity.
fn resolve_prewarm_concurrency() -> usize {
    static LOG_ONCE: OnceLock<()> = OnceLock::new();
    let capacity = match std::env::var(PREWARM_CONCURRENCY_ENV_VAR).ok().as_deref() {
        None | Some("") => PREWARM_CONCURRENCY_DEFAULT,
        Some(s) => match s.trim().parse::<usize>() {
            Ok(0) => {
                tracing::warn!(
                    env = PREWARM_CONCURRENCY_ENV_VAR,
                    value = s,
                    default = PREWARM_CONCURRENCY_DEFAULT,
                    "LUNARIS_PREWARM_CONCURRENCY=0 is invalid (would skip all warm-ups); \
                     using default"
                );
                PREWARM_CONCURRENCY_DEFAULT
            }
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    env = PREWARM_CONCURRENCY_ENV_VAR,
                    value = s,
                    default = PREWARM_CONCURRENCY_DEFAULT,
                    "LUNARIS_PREWARM_CONCURRENCY is not a valid positive integer; using default"
                );
                PREWARM_CONCURRENCY_DEFAULT
            }
        },
    };
    LOG_ONCE.get_or_init(|| {
        tracing::info!(
            target: "lunaris::handle",
            prewarm_concurrency = capacity,
            "prewarm_concurrency_resolved"
        );
    });
    capacity
}

static EMBEDDER_BACKEND_LOG_ONCE: OnceLock<()> = OnceLock::new();
static RERANKER_BACKEND_LOG_ONCE: OnceLock<()> = OnceLock::new();

/// Parsed embedder backend selection. Internal — the public surface is the
/// env var + `resolve_embedder()`. Lifted out for unit-testability without
/// triggering model construction / network I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbedderBackendKind {
    Fastembed,
    Candle,
    Ollama,
    /// Phase 22 — zero-vector backend. Used when no embedder feature is
    /// compiled in (silent fallback + one-shot warn) or when the operator
    /// explicitly sets `LUNARIS_EMBEDDER_BACKEND=noop`.
    Noop,
}

/// Default embedder backend selected when `LUNARIS_EMBEDDER_BACKEND` is
/// unset/empty. Picks the first compiled-in real backend in priority order
/// (`fastembed > candle > ollama`), falling through to `Noop` if no backend
/// feature is enabled.
const fn default_embedder_backend() -> EmbedderBackendKind {
    #[cfg(feature = "fastembed")]
    {
        EmbedderBackendKind::Fastembed
    }
    #[cfg(all(not(feature = "fastembed"), feature = "candle"))]
    {
        EmbedderBackendKind::Candle
    }
    #[cfg(all(not(feature = "fastembed"), not(feature = "candle"), feature = "ollama"))]
    {
        EmbedderBackendKind::Ollama
    }
    #[cfg(not(any(feature = "fastembed", feature = "candle", feature = "ollama")))]
    {
        EmbedderBackendKind::Noop
    }
}

/// Mirror of [`default_embedder_backend`] for the reranker: picks the first
/// compiled-in real backend in priority order (`fastembed > candle`), falling
/// through to `Noop` when neither is enabled.
const fn default_reranker_backend() -> RerankerBackendKind {
    #[cfg(feature = "fastembed")]
    {
        RerankerBackendKind::Fastembed
    }
    #[cfg(all(not(feature = "fastembed"), feature = "candle"))]
    {
        RerankerBackendKind::Candle
    }
    #[cfg(not(any(feature = "fastembed", feature = "candle")))]
    {
        RerankerBackendKind::Noop
    }
}

/// Parsed reranker backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerankerBackendKind {
    Fastembed,
    Candle,
    Noop,
}

/// Parse the embedder env-var value. Defaults to `Fastembed` for both unset
/// (`None`) and empty-string. Trims whitespace and matches case-insensitively
/// (mirrors `ConsolidatorPipelineHandle::backend_from_env`). Unknown values
/// produce `LunarisError::Storage(Backend(..))` with the `"is not one of"`
/// substring.
fn parse_embedder_backend(raw: Option<&str>) -> Result<EmbedderBackendKind, LunarisError> {
    let trimmed = raw.map(|v| v.trim());
    match trimmed {
        None | Some("") => Ok(default_embedder_backend()),
        Some(v) if v.eq_ignore_ascii_case("fastembed") => Ok(EmbedderBackendKind::Fastembed),
        Some(v) if v.eq_ignore_ascii_case("candle") => Ok(EmbedderBackendKind::Candle),
        Some(v) if v.eq_ignore_ascii_case("ollama") => Ok(EmbedderBackendKind::Ollama),
        Some(v) if v.eq_ignore_ascii_case("noop") => Ok(EmbedderBackendKind::Noop),
        Some(other) => Err(LunarisError::Storage(StorageError::Backend(format!(
            "{EMBEDDER_BACKEND_ENV_VAR}={other:?} is not one of [fastembed, candle, ollama, noop]; \
             unset for the build-dependent default (see EMBEDDER_BACKEND_ENV_VAR docs), \
             or set to one of those literals. \
             See docs/migration/0.2-to-0.3-optional-embedder.md."
        )))),
    }
}

/// Parse the reranker env-var value. Defaults to `Fastembed`. Same trim +
/// case-insensitive shape as [`parse_embedder_backend`].
fn parse_reranker_backend(raw: Option<&str>) -> Result<RerankerBackendKind, LunarisError> {
    let trimmed = raw.map(|v| v.trim());
    match trimmed {
        None | Some("") => Ok(default_reranker_backend()),
        Some(v) if v.eq_ignore_ascii_case("fastembed") => Ok(RerankerBackendKind::Fastembed),
        Some(v) if v.eq_ignore_ascii_case("candle") => Ok(RerankerBackendKind::Candle),
        Some(v) if v.eq_ignore_ascii_case("noop") => Ok(RerankerBackendKind::Noop),
        Some(other) => Err(LunarisError::Storage(StorageError::Backend(format!(
            "{RERANKER_BACKEND_ENV_VAR}={other:?} is not one of [fastembed, candle, noop]; \
             unset for the build-dependent default (see RERANKER_BACKEND_ENV_VAR docs), \
             or set to one of those literals. \
             See docs/migration/0.2-to-0.3-optional-embedder.md."
        )))),
    }
}

/// Phase 20-02 — resolve the default embedder for [`Lunaris::open`] from
/// [`EMBEDDER_BACKEND_ENV_VAR`]. Default `fastembed`; unknown values fail
/// fast (no silent fallback). Emits one `tracing::info!` per process on
/// first call (T-20-02-03).
///
/// Behaviour per backend:
/// - **fastembed** — [`lunaris_embed::FastembedEmbedder`]. On first call this
///   downloads the EmbeddingGemma 300M ONNX weights (~600 MB) from HF Hub
///   into `~/.cache/lunaris/models/fastembed/`. Subsequent calls hit the
///   cache. Construction errors (network unreachable + cold cache) propagate
///   — they are NOT silently downgraded to candle. Operators who need
///   air-gapped behaviour use `LUNARIS_EMBEDDER_BACKEND=candle` or build
///   with `--features candle-only`.
/// - **candle** — [`lunaris_embed::CandleEmbeddingGemma`] from
///   `~/.cache/lunaris/models/embedding-gemma-300m/`. Missing-weights error
///   surfaces the actionable `huggingface-cli` instruction from the
///   constructor.
/// - **ollama** — [`lunaris_embed::OllamaEmbedder`] pointing at
///   `http://localhost:11434`. Constructor is synchronous.
async fn resolve_embedder() -> Result<Arc<dyn Embedder>, LunarisError> {
    let raw = std::env::var(EMBEDDER_BACKEND_ENV_VAR).ok();
    let env_unset = matches!(raw.as_deref().map(str::trim), None | Some(""));
    let kind = parse_embedder_backend(raw.as_deref())?;
    let backend_name: &'static str = match kind {
        EmbedderBackendKind::Fastembed => "fastembed",
        EmbedderBackendKind::Candle => "candle",
        EmbedderBackendKind::Ollama => "ollama",
        EmbedderBackendKind::Noop => "noop",
    };
    EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
        // Phase 22 — silent-fallback warning: env unset AND we landed on
        // Noop because no real backend feature is compiled in. The operator
        // gets exactly one line per process so they know vector recall is
        // dead until they rebuild with a feature or call open_with_embedder.
        if env_unset && matches!(kind, EmbedderBackendKind::Noop) {
            tracing::warn!(
                target: "lunaris::handle",
                embedder_backend = backend_name,
                "no embedder backend compiled in — falling back to NoopEmbedder; \
                 vector recall will return empty rows. Rebuild with \
                 `--features fastembed` (or candle/ollama), set \
                 LUNARIS_EMBEDDER_BACKEND=noop to silence this warning, or \
                 pass a custom embedder via Lunaris::open_with_embedder."
            );
        } else {
            tracing::info!(
                target: "lunaris::handle",
                embedder_backend = backend_name,
                "embedder_backend_resolved"
            );
        }
    });
    match kind {
        #[cfg(feature = "fastembed")]
        EmbedderBackendKind::Fastembed => {
            let e = lunaris_embed::FastembedEmbedder::new(Default::default())?;
            Ok(Arc::new(e) as Arc<dyn Embedder>)
        }
        #[cfg(not(feature = "fastembed"))]
        EmbedderBackendKind::Fastembed => {
            Err(LunarisError::Storage(StorageError::Backend(format!(
                "{EMBEDDER_BACKEND_ENV_VAR}=fastembed but this build was compiled without the \
                 `fastembed` feature. Rebuild without `--no-default-features` or pass \
                 `--features fastembed`, OR set {EMBEDDER_BACKEND_ENV_VAR}=candle to use the \
                 candle backend (this is the expected air-gapped path; see \
                 docs/migration/0.1-to-0.2-fastembed-default.md)."
            ))))
        }
        #[cfg(feature = "candle")]
        EmbedderBackendKind::Candle => {
            let e = lunaris_embed::CandleEmbeddingGemma::new(Default::default()).await?;
            Ok(Arc::new(e) as Arc<dyn Embedder>)
        }
        #[cfg(not(feature = "candle"))]
        EmbedderBackendKind::Candle => Err(LunarisError::Storage(StorageError::Backend(format!(
            "{EMBEDDER_BACKEND_ENV_VAR}=candle but this build was compiled without the \
                 `candle` feature. Rebuild with `--features candle` or unset the env var to use \
                 the fastembed default."
        )))),
        #[cfg(feature = "ollama")]
        EmbedderBackendKind::Ollama => {
            let e = lunaris_embed::OllamaEmbedder::new(Default::default())?;
            Ok(Arc::new(e) as Arc<dyn Embedder>)
        }
        #[cfg(not(feature = "ollama"))]
        EmbedderBackendKind::Ollama => Err(LunarisError::Storage(StorageError::Backend(format!(
            "{EMBEDDER_BACKEND_ENV_VAR}=ollama but this build was compiled without the \
                 `ollama` feature. Rebuild with `--features ollama` or unset the env var."
        )))),
        // Phase 22 — zero-vector backend, always available (no feature gate).
        // Dim is sourced from LUNARIS_EMBED_DIM; non-numeric/0/unset all
        // resolve to NOOP_DEFAULT_DIM (768) with a warn from `resolve_embed_dim`.
        EmbedderBackendKind::Noop => {
            let dim = resolve_embed_dim();
            Ok(Arc::new(lunaris_embed::NoopEmbedder::new(dim)) as Arc<dyn Embedder>)
        }
    }
}

/// Phase 22 — resolve the noop embedder dim from [`EMBED_DIM_ENV_VAR`].
/// Positive integer; `0`, non-numeric, and unset all return
/// [`lunaris_embed::NOOP_DEFAULT_DIM`] with a `tracing::warn!` for the
/// non-default-but-invalid cases.
fn resolve_embed_dim() -> usize {
    static LOG_ONCE: OnceLock<()> = OnceLock::new();
    let dim = match std::env::var(EMBED_DIM_ENV_VAR).ok().as_deref() {
        None | Some("") => lunaris_embed::NOOP_DEFAULT_DIM,
        Some(s) => match s.trim().parse::<usize>() {
            Ok(0) => {
                tracing::warn!(
                    env = EMBED_DIM_ENV_VAR,
                    value = s,
                    default = lunaris_embed::NOOP_DEFAULT_DIM,
                    "LUNARIS_EMBED_DIM=0 is invalid (storage rejects dim=0); using default"
                );
                lunaris_embed::NOOP_DEFAULT_DIM
            }
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    env = EMBED_DIM_ENV_VAR,
                    value = s,
                    default = lunaris_embed::NOOP_DEFAULT_DIM,
                    "LUNARIS_EMBED_DIM is not a valid positive integer; using default"
                );
                lunaris_embed::NOOP_DEFAULT_DIM
            }
        },
    };
    LOG_ONCE.get_or_init(|| {
        tracing::info!(
            target: "lunaris::handle",
            embed_dim = dim,
            "embed_dim_resolved"
        );
    });
    dim
}

/// Phase 20-02 — resolve the default reranker for [`Lunaris::open`] from
/// [`RERANKER_BACKEND_ENV_VAR`]. Default `fastembed`; unknown values fail fast.
///
/// Behaviour per backend:
/// - **fastembed** — [`lunaris_rerank::FastembedReranker`]. HF Hub auto-download
///   on first call. Construction errors propagate.
/// - **candle** — [`lunaris_rerank::BgeRerankerV2M3::try_new_from_default_cache`].
///   On cache miss falls back to [`NoopReranker`] with `tracing::warn!`
///   (RETRIEVE-06 contract — recall still runs end-to-end without the
///   12 ms cross-encoder pass). This fallback is **specific to the candle
///   branch**; fastembed errors are NOT silently downgraded.
/// - **noop** — [`NoopReranker`]. Always available; no feature gate. Operators
///   pin this to skip the rerank pass entirely on budget-constrained hardware.
async fn resolve_reranker() -> Result<Arc<dyn Reranker>, LunarisError> {
    let raw = std::env::var(RERANKER_BACKEND_ENV_VAR).ok();
    let kind = parse_reranker_backend(raw.as_deref())?;
    let backend_name: &'static str = match kind {
        RerankerBackendKind::Fastembed => "fastembed",
        RerankerBackendKind::Candle => "candle",
        RerankerBackendKind::Noop => "noop",
    };
    RERANKER_BACKEND_LOG_ONCE.get_or_init(|| {
        tracing::info!(
            target: "lunaris::handle",
            reranker_backend = backend_name,
            "reranker_backend_resolved"
        );
    });
    match kind {
        #[cfg(feature = "fastembed")]
        RerankerBackendKind::Fastembed => {
            let r = lunaris_rerank::FastembedReranker::new(Default::default())?;
            Ok(Arc::new(r) as Arc<dyn Reranker>)
        }
        #[cfg(not(feature = "fastembed"))]
        RerankerBackendKind::Fastembed => {
            Err(LunarisError::Storage(StorageError::Backend(format!(
                "{RERANKER_BACKEND_ENV_VAR}=fastembed but this build was compiled without the \
                 `fastembed` feature. Set {RERANKER_BACKEND_ENV_VAR}=candle for the candle \
                 cross-encoder, or =noop to skip the rerank pass. See \
                 docs/migration/0.1-to-0.2-fastembed-default.md."
            ))))
        }
        #[cfg(feature = "candle")]
        RerankerBackendKind::Candle => {
            match lunaris_rerank::BgeRerankerV2M3::try_new_from_default_cache().await {
                Ok(r) => Ok(Arc::new(r) as Arc<dyn Reranker>),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "bge-reranker-v2-m3 unavailable; using NoopReranker (recall budget skips the rerank pass per RETRIEVE-06 contract — install weights via `python -m huggingface_hub download BAAI/bge-reranker-v2-m3 --local-dir ~/.cache/lunaris/models/bge-reranker-v2-m3`)"
                    );
                    Ok(Arc::new(NoopReranker) as Arc<dyn Reranker>)
                }
            }
        }
        #[cfg(not(feature = "candle"))]
        RerankerBackendKind::Candle => Err(LunarisError::Storage(StorageError::Backend(format!(
            "{RERANKER_BACKEND_ENV_VAR}=candle but this build was compiled without the \
                 `candle` feature. Rebuild with `--features candle` or unset for fastembed default."
        )))),
        RerankerBackendKind::Noop => Ok(Arc::new(NoopReranker) as Arc<dyn Reranker>),
    }
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

/// Plan 04-04 + RFC 0006: Construct the default verifier for [`Lunaris::open`].
///
/// Backend resolved from `LUNARIS_VERIFIER_BACKEND`:
/// - unset OR `"270m"` / `"small"` → [`lunaris_verify::CandleGemma3_270M`]
///   when the `verify-small` feature is on. This is the **RFC 0006 laptop
///   floor**: ~600 MB weights, ~1 GB RAM, runs on a dev laptop.
/// - `"27b"` / `"large"` → [`lunaris_verify::CandleGemma3_27B`] when the
///   `verify-large` (or compatibility-alias `candle`) feature is on. The
///   legacy default; ~14 GiB weights, ~24 GB RAM.
/// - `"noop"` → [`NoopVerifier`] (operator opt-out).
/// - anything else → fail-soft to [`NoopVerifier`] with a `tracing::warn!`
///   naming the bad value (matches `default_extractor` cache-miss shape).
///
/// On cache-miss for the resolved backend (the common case on dev boxes
/// without weights pre-downloaded) emits a `tracing::warn!` and
/// substitutes [`NoopVerifier`] per the D-02 default-OFF contract.
///
/// Callers wire their own verifier via [`Lunaris::with_verifier`].
#[cfg(feature = "candle")]
async fn default_verifier() -> Arc<dyn Verifier> {
    let raw = std::env::var("LUNARIS_VERIFIER_BACKEND").unwrap_or_default();
    let backend = raw.trim().to_ascii_lowercase();
    match backend.as_str() {
        "" | "270m" | "small" => default_verifier_270m().await,
        "27b" | "large" => default_verifier_27b().await,
        "noop" => Arc::new(NoopVerifier) as Arc<dyn Verifier>,
        other => {
            tracing::warn!(
                backend = %other,
                "LUNARIS_VERIFIER_BACKEND unrecognised — falling back to NoopVerifier (valid: 270m, 27b, noop)"
            );
            Arc::new(NoopVerifier) as Arc<dyn Verifier>
        }
    }
}

#[cfg(all(feature = "candle", feature = "verify-small"))]
async fn default_verifier_270m() -> Arc<dyn Verifier> {
    match lunaris_verify::CandleGemma3_270M::new(Default::default()).await {
        Ok(v) => Arc::new(v) as Arc<dyn Verifier>,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "gemma-3-270m-it unavailable; using NoopVerifier (install weights via `huggingface-cli download google/gemma-3-270m-it --local-dir ~/.cache/lunaris/models/gemma-3-270m-it`)"
            );
            Arc::new(NoopVerifier) as Arc<dyn Verifier>
        }
    }
}

/// Without `verify-small`, fall back to 27B so a bare `candle` build still
/// gets a real verifier when one is in cache. Matches the v0.2.0 behaviour
/// for callers who haven't opted into the laptop floor.
#[cfg(all(feature = "candle", not(feature = "verify-small")))]
async fn default_verifier_270m() -> Arc<dyn Verifier> {
    tracing::debug!(
        "LUNARIS_VERIFIER_BACKEND=270m requested but `verify-small` feature is off; falling back to 27B"
    );
    default_verifier_27b().await
}

#[cfg(feature = "candle")]
async fn default_verifier_27b() -> Arc<dyn Verifier> {
    match lunaris_verify::CandleGemma3_27B::new(Default::default()).await {
        Ok(v) => Arc::new(v) as Arc<dyn Verifier>,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "gemma-3-27b-it unavailable; using NoopVerifier (install weights via `huggingface-cli download google/gemma-3-27b-it --local-dir ~/.cache/lunaris/models/gemma-3-27b-it`)"
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

/// Plan 04-04 + Phase 16-01 (CONSOL-V1-01): Construct the default consolidator
/// for [`Lunaris::open`], resolving from
/// [`ConsolidatorPipelineHandle::BACKEND_ENV_VAR`] (`LUNARIS_CONSOLIDATOR_BACKEND`).
///
/// Default (env unset) → [`lunaris_consolidate::ActRConsolidator`] (production
/// default per CONSOL-V1-01). Operators opt out to [`NoopConsolidator`] by
/// setting `LUNARIS_CONSOLIDATOR_BACKEND=noop` (preserved third toggle surface:
/// code override via [`Lunaris::with_consolidator`] also still works).
///
/// Unknown env values fail-fast via [`LunarisError::Storage`]
/// (`StorageError::Backend`) — NO silent fallback.
fn default_consolidator() -> Result<Arc<dyn Consolidator>, LunarisError> {
    ConsolidatorPipelineHandle::backend_from_env()
}

// ── Phase 20-02 unit tests — env-var parsing only ──────────────────────────
//
// We deliberately do NOT construct `FastembedEmbedder` / `CandleEmbeddingGemma`
// in unit tests because both backends do I/O (HF Hub download / cache disk
// read) which would make `cargo test -p lunaris --lib` hit the network. The
// `resolve_*` async wrappers are exercised by the recipe integration suite
// (Plan 20-02 Task 4 smoke); here we cover the parse / unknown-value /
// case-and-whitespace behaviour exhaustively.
#[cfg(test)]
mod backend_resolution_tests {
    use super::*;

    #[test]
    fn embedder_default_when_unset() {
        // Phase 22 — default depends on the compiled feature set; the
        // priority order is fastembed > candle > ollama > noop. We assert
        // the priority via `default_embedder_backend()` rather than pin a
        // single variant so the test matrix tracks the build matrix.
        assert_eq!(parse_embedder_backend(None).unwrap(), default_embedder_backend());
    }

    #[test]
    fn embedder_default_when_empty() {
        assert_eq!(parse_embedder_backend(Some("")).unwrap(), default_embedder_backend());
        assert_eq!(parse_embedder_backend(Some("   ")).unwrap(), default_embedder_backend());
    }

    #[test]
    fn embedder_explicit_noop() {
        assert_eq!(parse_embedder_backend(Some("noop")).unwrap(), EmbedderBackendKind::Noop);
        assert_eq!(parse_embedder_backend(Some("NOOP")).unwrap(), EmbedderBackendKind::Noop);
        assert_eq!(parse_embedder_backend(Some("  Noop  ")).unwrap(), EmbedderBackendKind::Noop);
    }

    #[test]
    fn embedder_default_falls_back_to_noop_when_no_real_backend() {
        // Compile-time assertion: when no real embedder feature is enabled,
        // `default_embedder_backend()` MUST yield `Noop` so `Lunaris::open()`
        // never fails for "no backend compiled" reasons. The test only fires
        // in the matching feature configuration; in feature-rich builds it
        // is silently filtered.
        #[cfg(not(any(feature = "fastembed", feature = "candle", feature = "ollama")))]
        {
            assert_eq!(default_embedder_backend(), EmbedderBackendKind::Noop);
        }
    }

    #[test]
    fn embedder_explicit_fastembed() {
        assert_eq!(
            parse_embedder_backend(Some("fastembed")).unwrap(),
            EmbedderBackendKind::Fastembed
        );
        assert_eq!(
            parse_embedder_backend(Some("FASTEMBED")).unwrap(),
            EmbedderBackendKind::Fastembed
        );
        assert_eq!(
            parse_embedder_backend(Some("  Fastembed  ")).unwrap(),
            EmbedderBackendKind::Fastembed
        );
    }

    #[test]
    fn embedder_explicit_candle() {
        assert_eq!(parse_embedder_backend(Some("candle")).unwrap(), EmbedderBackendKind::Candle);
        assert_eq!(parse_embedder_backend(Some("Candle")).unwrap(), EmbedderBackendKind::Candle);
    }

    #[test]
    fn embedder_explicit_ollama() {
        assert_eq!(parse_embedder_backend(Some("ollama")).unwrap(), EmbedderBackendKind::Ollama);
    }

    #[test]
    fn embedder_unknown_fails_fast() {
        let err = parse_embedder_backend(Some("bogus")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("is not one of"), "msg should contain 'is not one of': {msg}");
        assert!(msg.contains("bogus"), "msg should echo the bad value: {msg}");
        assert!(msg.contains("LUNARIS_EMBEDDER_BACKEND"), "msg should name the env var: {msg}");
    }

    #[test]
    fn reranker_default_when_unset() {
        assert_eq!(parse_reranker_backend(None).unwrap(), default_reranker_backend());
    }

    #[test]
    fn reranker_default_when_empty() {
        assert_eq!(parse_reranker_backend(Some("")).unwrap(), default_reranker_backend());
    }

    #[test]
    fn reranker_explicit_fastembed() {
        assert_eq!(
            parse_reranker_backend(Some("fastembed")).unwrap(),
            RerankerBackendKind::Fastembed
        );
    }

    #[test]
    fn reranker_explicit_candle() {
        assert_eq!(parse_reranker_backend(Some("candle")).unwrap(), RerankerBackendKind::Candle);
    }

    #[test]
    fn reranker_explicit_noop() {
        assert_eq!(parse_reranker_backend(Some("noop")).unwrap(), RerankerBackendKind::Noop);
        assert_eq!(parse_reranker_backend(Some("NOOP")).unwrap(), RerankerBackendKind::Noop);
    }

    #[test]
    fn reranker_unknown_fails_fast() {
        let err = parse_reranker_backend(Some("xyzzy")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("is not one of"), "msg should contain 'is not one of': {msg}");
        assert!(msg.contains("xyzzy"));
        assert!(msg.contains("LUNARIS_RERANKER_BACKEND"));
    }
}

// ── Phase 13 unit tests — end_turn / ReflectSupervisor wire-up ──────────────
//
// All tests use stub storage + `StubEmbedder` from lunaris_core (proven by
// the existing verify_pipeline_smoke integration tests) so no I/O is
// performed. The three tests cover:
//   1. Default Noop supervisor → empty ReflectOutput.
//   2. Custom stub supervisor → output propagates; input fields thread through.
//   3. Supervisor returning Err → end_turn propagates the error.
#[cfg(test)]
mod end_turn_tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::{self, BoxStream};
    use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
    use lunaris_core::storage::types::{
        CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
    };
    use lunaris_core::{
        CypherDialect, HlcClock, LunarisError, Scope, StorageCapabilities, StorageError,
        StoragePort, StubEmbedder,
    };
    use lunaris_verify::{ReflectInput, ReflectOutput, ReflectSupervisor};
    use std::sync::Arc;
    use ulid::Ulid;

    // ── minimal stub storage (matches actual StoragePort signatures) ──────────

    struct NullStorage;

    #[async_trait]
    impl StoragePort for NullStorage {
        async fn atomic_write(
            &self,
            _scope: &Scope,
            _ops: &[WriteOp],
        ) -> Result<Lsn, StorageError> {
            Ok(Lsn { wall_ms: 1, counter: 0 })
        }

        async fn read_as_of(
            &self,
            _scope: &Scope,
            _key: &[u8],
            _as_of: lunaris_core::Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            Ok(None)
        }

        async fn vector_search(
            &self,
            _scope: &Scope,
            _index: &str,
            _query: &[f32],
            _k: usize,
            _filter: Option<&Filter>,
            _as_of: Option<lunaris_core::Hlc>,
            _rerank: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            Ok(vec![])
        }

        async fn graph_traverse(
            &self,
            _scope: &Scope,
            _q: &CypherQuery,
            _as_of: Option<lunaris_core::Hlc>,
        ) -> Result<GraphResult, StorageError> {
            Ok(GraphResult::default())
        }

        async fn scan_range(
            &self,
            _scope: &Scope,
            _prefix: &[u8],
            _as_of: Option<lunaris_core::Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            Ok(Box::pin(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new())))
        }

        async fn publish(
            &self,
            _scope: &Scope,
            _topic: &str,
            _partition: u16,
            _payload: Bytes,
        ) -> Result<u64, StorageError> {
            Ok(0)
        }

        async fn subscribe(
            &self,
            _scope: &Scope,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            Ok(Box::pin(stream::empty()))
        }

        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities {
                bi_temporal_native: false,
                graph_native: false,
                rerank_native: false,
                queue_native: false,
                max_vector_dim: 768,
                native_rrf: false,
                max_scopes_recommended: 0,
                cypher_dialect: CypherDialect::Legacy,
            }
        }
    }

    #[async_trait]
    impl KeywordPort for NullStorage {
        async fn keyword_search(
            &self,
            _scope: &Scope,
            _index: &str,
            _query: &str,
            _k: usize,
            _filter: Option<&Filter>,
            _as_of: Option<lunaris_core::Hlc>,
        ) -> Result<Vec<KeywordHit>, StorageError> {
            Ok(vec![])
        }
    }

    fn make_handle() -> Lunaris {
        // HlcClock::new already returns Arc<HlcClock> — no extra Arc::new wrap.
        let storage: Arc<dyn StoragePort> = Arc::new(NullStorage);
        let keyword: Arc<dyn KeywordPort> = Arc::new(NullStorage);
        let embedder = Arc::new(StubEmbedder::new(4));
        let clock = HlcClock::new(0);
        Lunaris::with_parts_keyword(storage, keyword, embedder, clock)
    }

    // ── test 1: default noop supervisor → empty output ────────────────────────

    #[tokio::test]
    async fn end_turn_noop_returns_empty_output() {
        let handle = make_handle();
        // Default is NoopReflectSupervisor — applies() = false.
        assert!(!handle.reflect_supervisor().applies());

        let input = ReflectInput {
            turn_id: Some(Ulid::new()),
            turn_summary: "agent answered a question".into(),
            recent_fact_ids: vec![Ulid::new()],
            recent_chunk_ids: vec![Ulid::new()],
        };
        let out = handle.end_turn(input).await.unwrap();
        assert_eq!(out, ReflectOutput::default());
        assert!(out.invalidate.is_empty());
        assert!(out.boost.is_empty());
        assert!(out.pre_warm_query.is_none());
    }

    // ── test 2: custom stub supervisor → output + input fields thread through ─

    /// Captures the input it received so the test can assert field propagation.
    struct CapturingReflectSupervisor {
        output: ReflectOutput,
        captured: parking_lot::Mutex<Option<ReflectInput>>,
    }

    #[async_trait]
    impl ReflectSupervisor for CapturingReflectSupervisor {
        async fn reflect(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
            *self.captured.lock() = Some(input);
            Ok(self.output.clone())
        }
        fn applies(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn end_turn_stub_supervisor_propagates_output_and_input() {
        let fact_id = Ulid::new();
        let chunk_id = Ulid::new();
        let turn_id = Ulid::new();
        let expected_output = ReflectOutput {
            invalidate: vec![fact_id],
            boost: vec![chunk_id],
            pre_warm_query: Some("what is Alice's role?".into()),
        };
        let supervisor = Arc::new(CapturingReflectSupervisor {
            output: expected_output.clone(),
            captured: parking_lot::Mutex::new(None),
        });

        let handle = make_handle().with_reflect_supervisor(supervisor.clone());
        assert!(handle.reflect_supervisor().applies());

        let input = ReflectInput {
            turn_id: Some(turn_id),
            turn_summary: "turn summary text".into(),
            recent_fact_ids: vec![fact_id],
            recent_chunk_ids: vec![chunk_id],
        };
        let out = handle.end_turn(input).await.unwrap();
        assert_eq!(out, expected_output);

        // Confirm the supervisor received the exact input we passed.
        let captured = supervisor.captured.lock().take().unwrap();
        assert_eq!(captured.turn_id, Some(turn_id));
        assert_eq!(captured.recent_fact_ids, vec![fact_id]);
        assert_eq!(captured.recent_chunk_ids, vec![chunk_id]);
        assert_eq!(captured.turn_summary, "turn summary text");
    }

    // ── test 3: supervisor returns Err → end_turn propagates error ────────────

    struct ErrReflectSupervisor;

    #[async_trait]
    impl ReflectSupervisor for ErrReflectSupervisor {
        async fn reflect(&self, _input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
            Err(LunarisError::Storage(StorageError::NotSupported("reflect budget exhausted")))
        }
    }

    #[tokio::test]
    async fn end_turn_propagates_supervisor_error() {
        let handle = make_handle().with_reflect_supervisor(Arc::new(ErrReflectSupervisor));
        let result = handle.end_turn(ReflectInput::default()).await;
        assert!(result.is_err(), "end_turn must propagate supervisor error");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("reflect budget exhausted"), "error message: {msg}");
    }
}
