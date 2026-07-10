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
//!   constructs the default embedder
//!   ([`lunaris_embed_native::NativeEmbedder`] backed by granite-r2 — v0.4
//!   N-03 cutover) and a fresh `HlcClock(node_id=0)`.
//! - [`Lunaris::with_parts`] — escape hatch for tests + the Plan 02-01
//!   latency-budget swap. Lets callers wire any `Arc<dyn StoragePort>` and
//!   `Arc<dyn Embedder>` directly. Used by the Phase 2 ingest smoke test
//!   (in-memory recording storage + `StubEmbedder`).
//! - [`Lunaris::with_embedder`] — public escape hatch to replace the
//!   embedder on an already-constructed handle (e.g., swap from
//!   `NativeEmbedder` to the feature-gated `lunaris_embed_remote::OllamaEmbedder`
//!   or to a BYO `Arc<dyn Embedder>`).
//!
//! ## Invariant
//!
//! `Lunaris` does NOT cache mutable per-call retrieval state. Every call
//! constructs a fresh borrow of the shared Arcs, so the same handle is safe to
//! use from multiple tokio tasks concurrently. The production constructor wraps
//! the embedder in a small exact-text LRU cache so repeated agent prompts and
//! repeated chunk text do not re-run model inference.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use lunaris_consolidate::Consolidator;
use lunaris_core::{
    Embedder, HlcClock, KeywordPort, Lsn, LunarisError, Scope, StorageError, StoragePort,
};
use lunaris_ingest::{BakoffConfig, TokenCounter, make_token_counter};
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
    /// BPE token counter for the ingest chunker (CHUNK-01 / Finding 1 fix).
    ///
    /// Loaded from the embedder model directory (`embedder_dir()/tokenizer.json`)
    /// at `open` time via `make_token_counter`. Falls back to
    /// `SurrogateTokenCounter` (words×1.3) when the file is absent or
    /// malformed — `tracing::warn!` is emitted in that case. The `with_parts`
    /// and `with_parts_keyword` test seams always use the surrogate so tests
    /// have no model-artifact dependency.
    ///
    /// Passed to `ingest_episode_with_counter` so production chunking uses
    /// real BPE token counts rather than the v0 heuristic.
    pub(crate) token_counter: Arc<dyn TokenCounter + Send + Sync>,
    /// Phase 28 — adaptive meta-framework bake-off config.
    ///
    /// When `Some`, [`Lunaris::ingest`] routes through
    /// [`lunaris_ingest::ingest_episode_with_bakeoff`] which runs the multi-generator
    /// bake-off and persists the winning candidate. The winner's scoring embeddings
    /// are reused directly (SINGLE-PASS — no re-embed). When `None` (default),
    /// the standard [`lunaris_ingest::ingest_episode_with_counter`] path is used.
    ///
    /// Install via [`Self::with_bakeoff`]. `Arc` allows cheap clone of the handle
    /// without copying the config on every ingest call.
    pub(crate) bakeoff_config: Option<Arc<BakoffConfig>>,
}

struct CachedEmbedder {
    inner: Arc<dyn Embedder>,
    cache: parking_lot::RwLock<lru::LruCache<String, Vec<f32>>>,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

impl CachedEmbedder {
    fn new(inner: Arc<dyn Embedder>, capacity: NonZeroUsize) -> Self {
        Self {
            inner,
            cache: parking_lot::RwLock::new(lru::LruCache::new(capacity)),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for CachedEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedEmbedder")
            .field("dim", &self.inner.dim())
            .field("cache_len", &self.cache.read().len())
            .field("hits", &self.hits.load(Ordering::Relaxed))
            .field("misses", &self.misses.load(Ordering::Relaxed))
            .finish()
    }
}

#[async_trait::async_trait]
impl Embedder for CachedEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        let mut out: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
        let mut missing: HashMap<String, Vec<usize>> = HashMap::new();

        {
            let cache = self.cache.read();
            for (idx, input) in inputs.iter().enumerate() {
                if let Some(cached) = cache.peek(*input) {
                    out[idx] = Some(cached.clone());
                    self.hits.fetch_add(1, Ordering::Relaxed);
                } else {
                    missing.entry((*input).to_string()).or_default().push(idx);
                }
            }
        }

        if !missing.is_empty() {
            let keys: Vec<String> = missing.keys().cloned().collect();
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let embedded = self.inner.embed_batch(&refs).await?;
            if embedded.len() != keys.len() {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "cached embedder inner returned {} rows for {} inputs",
                    embedded.len(),
                    keys.len()
                ))));
            }

            let mut cache = self.cache.write();
            for (key, embedding) in keys.into_iter().zip(embedded.into_iter()) {
                self.misses.fetch_add(1, Ordering::Relaxed);
                cache.put(key.clone(), embedding.clone());
                if let Some(indices) = missing.remove(&key) {
                    for idx in indices {
                        out[idx] = Some(embedding.clone());
                    }
                }
            }
        }

        out.into_iter()
            .map(|row| {
                row.ok_or_else(|| {
                    LunarisError::Storage(StorageError::Backend(
                        "cached embedder failed to fill an output row".into(),
                    ))
                })
            })
            .collect()
    }
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
    /// ## Default backend resolution (v0.4 native)
    ///
    /// - **Embedder** — [`lunaris_embed_native::NativeEmbedder`] backed by
    ///   `ibm-granite/granite-embedding-311m-multilingual-r2` (FP16, 768-d).
    ///   Loaded from
    ///   `~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/`
    ///   (override via `LUNARIS_EMBEDDER_DIR`). Missing weights → fail-fast
    ///   with an actionable `huggingface-cli` instruction. Operators on
    ///   v0.4-quantized builds (`--features embedder-gguf`) can point
    ///   `LUNARIS_EMBEDDER_GGUF=<path/to/granite-r2.Q4_K_M.gguf>` to load
    ///   `NativeQuantizedEmbedder` instead.
    /// - **Reranker** — [`lunaris_rerank_native::NativeReranker`] backed by
    ///   `BAAI/bge-reranker-v2-m3` (FP32, sigmoid scores ∈ [0, 1]). Loaded
    ///   from `~/.cache/lunaris/models/bge-reranker-v2-m3/` (override via
    ///   `LUNARIS_RERANKER_DIR`). Cache miss → `tracing::warn!` +
    ///   [`NoopReranker`] (RETRIEVE-06 contract: recall path runs even
    ///   without the rerank pass). Operators on quantized builds
    ///   (`--features reranker-gguf`) can point
    ///   `LUNARIS_RERANKER_GGUF=<path/to/bge-reranker.Q4_K_M.gguf>` to load
    ///   `NativeQuantizedReranker`.
    /// - **Verifier / Consolidator** — unchanged. Resolved from
    ///   `LUNARIS_VERIFIER_BACKEND` / `LUNARIS_CONSOLIDATOR_BACKEND` (see
    ///   `default_verifier`, `default_consolidator`).
    /// - **Air-gap escape hatch** — build with `--features embed-remote` and
    ///   set `LUNARIS_EMBEDDER_OLLAMA_URL=<endpoint>` to route the embedder
    ///   through an existing Ollama instance via
    ///   [`lunaris_embed_remote::OllamaEmbedder`]. NOT the supported path;
    ///   logged as a runtime warn.
    ///
    /// See `docs/migration/0.3-to-0.4-native-default.md` for the full
    /// migration recipe.
    pub async fn open(url: &str) -> Result<Self, LunarisError> {
        let embedder = resolve_embedder().await?;
        Self::open_with_embedder(url, embedder).await
    }

    /// Like [`Lunaris::open`] but uses the caller-provided `embedder`
    /// directly instead of constructing the default `NativeEmbedder` /
    /// `NoopEmbedder` fallback.
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
    ///   use lunaris_core::NoopEmbedder;
    ///   let handle = Lunaris::open_with_embedder(
    ///       "moon://localhost:6380",
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
        let embedder = maybe_cached_embedder(embedder);
        let scheme = url.split("://").next().unwrap_or("");
        let clock = HlcClock::new(0);
        // Build the BPE token counter from the embedder's tokenizer.json.
        // Falls back to SurrogateTokenCounter (tracing::warn!) when absent.
        let token_counter = make_token_counter(Some(&embedder_dir().join("tokenizer.json")));
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
                // dimension (default 768-d for granite-r2; pass a wider embedder
                // via `Lunaris::open_with_embedder` and the indices grow to
                // match). Moon's FT.CREATE has no dimension cap. Footgun: if
                // the Moon instance already holds indices at a different dim,
                // they are NOT auto-resized — drop them first.
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
                    boost_cache: Arc::new(parking_lot::RwLock::new(lru::LruCache::new(
                        boost_cache_capacity(),
                    ))),
                    warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                        resolve_prewarm_concurrency(),
                    )),
                    token_counter: token_counter.clone(),
                    bakeoff_config: None,
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
                    boost_cache: Arc::new(parking_lot::RwLock::new(lru::LruCache::new(
                        boost_cache_capacity(),
                    ))),
                    warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                        resolve_prewarm_concurrency(),
                    )),
                    token_counter: token_counter.clone(),
                    bakeoff_config: None,
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
                    boost_cache: Arc::new(parking_lot::RwLock::new(lru::LruCache::new(
                        boost_cache_capacity(),
                    ))),
                    warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(
                        resolve_prewarm_concurrency(),
                    )),
                    token_counter: token_counter.clone(),
                    bakeoff_config: None,
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
            boost_cache: Arc::new(parking_lot::RwLock::new(lru::LruCache::new(
                boost_cache_capacity(),
            ))),
            // Phase 14.3 — semaphore for bounded fire-and-forget warm-up spawns.
            warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(resolve_prewarm_concurrency())),
            // Test seam: no model artifact available; use the surrogate counter.
            token_counter: make_token_counter(None),
            // Phase 28: bakeoff OFF by default in test seam; install via with_bakeoff.
            bakeoff_config: None,
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
            boost_cache: Arc::new(parking_lot::RwLock::new(lru::LruCache::new(
                boost_cache_capacity(),
            ))),
            // Phase 14.3 — semaphore for bounded fire-and-forget warm-up spawns.
            warm_up_semaphore: Arc::new(tokio::sync::Semaphore::new(resolve_prewarm_concurrency())),
            // Test seam: no model artifact available; use the surrogate counter.
            token_counter: make_token_counter(None),
            // Phase 28: bakeoff OFF by default in test seam; install via with_bakeoff.
            bakeoff_config: None,
        }
    }

    /// Public escape hatch — replace the embedder on an existing handle.
    ///
    /// [`Lunaris::open`] constructs the default `NativeEmbedder` backed by
    /// granite-r2 (v0.4 N-03 cutover); call this method post-construction
    /// to swap in any `Arc<dyn Embedder>` (e.g., a `StubEmbedder` in tests,
    /// the feature-gated `lunaris_embed_remote::OllamaEmbedder`, or a remote
    /// embedder service).
    ///
    /// **Footgun**: this method does NOT re-size the underlying storage
    /// vector index. If you swap embedders post-`open()`, ensure the new
    /// embedder's `dim()` matches the original; otherwise `FT.SEARCH` /
    /// `pgvector` queries will reject the dimension mismatch at call time.
    /// Use [`Lunaris::open_with_embedder`] for the pre-index-creation path.
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        if self.embedder.dim() != embedder.dim() {
            tracing::warn!(
                target: "lunaris::handle",
                store_dim = self.embedder.dim(),
                new_dim = embedder.dim(),
                "with_embedder: dim mismatch — silently swapping; vector index is sized for store_dim. \
                 Use try_with_embedder() to refuse the swap, or open_with_embedder() for a fresh handle."
            );
        }
        self.embedder = maybe_cached_embedder(embedder);
        self
    }

    /// Phase 28 — install an adaptive meta-framework bake-off config.
    ///
    /// When installed, every subsequent [`Lunaris::ingest`] call routes through
    /// [`lunaris_ingest::ingest_episode_with_bakeoff`], which runs the
    /// multi-generator bake-off and persists the winning candidate. The winner's
    /// scoring embeddings are reused directly (SINGLE-PASS — no re-embed).
    ///
    /// Pass `None` (or call this with `Arc::new(BakoffConfig::default())`) to
    /// restore the standard counter-based ingest path. The `Arc` wrapper lets
    /// the config be shared cheaply across `Lunaris::clone()` calls.
    ///
    /// ## INGEST-04 invariant
    ///
    /// Installing a bakeoff config does NOT add a second `atomic_write` call.
    /// Both the standard path and the bakeoff path funnel through
    /// `assemble_and_write` in `lunaris_ingest::pipeline`, which holds the
    /// single executable `storage.atomic_write` call site.
    pub fn with_bakeoff(mut self, config: Arc<BakoffConfig>) -> Self {
        self.bakeoff_config = Some(config);
        self
    }

    /// N-04 D2 — fallible counterpart to [`Self::with_embedder`].
    ///
    /// Refuses the swap when `embedder.dim() != self.embedder.dim()`. The
    /// handle's existing `embedder.dim()` is the dim Moon's `FT.CREATE` /
    /// Postgres's `pgvector` column was sized for at `Lunaris::open*` time
    /// (see [`Lunaris::open_with_embedder`] — the dim flows into
    /// `MoonStorage::connect_with_dim`). Replacing it with a different-width
    /// embedder produces garbage similarity scores until the index is
    /// rebuilt, which is silent corruption masquerading as a working
    /// recall path. This method exposes the check at the API boundary so
    /// callers can either match the dim or migrate explicitly.
    ///
    /// Returns `Ok(Self)` on match, otherwise
    /// `Err(LunarisError::Storage(StorageError::Backend(_)))` carrying
    /// both dims in the message.
    ///
    /// The infallible [`Self::with_embedder`] is intentionally retained
    /// (and emits a `tracing::warn!` on mismatch) for backwards-compat with
    /// callers that have proven their backend tolerates the swap (e.g.,
    /// `memory://` tests that never run a vector query).
    pub fn try_with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Result<Self, LunarisError> {
        let store_dim = self.embedder.dim();
        let new_dim = embedder.dim();
        if store_dim != new_dim {
            return Err(LunarisError::Storage(lunaris_core::StorageError::Backend(format!(
                "embedder dim {new_dim} != store dim {store_dim}; drop and re-open with \
                 matching config or migrate (no auto-resize — vectors at the storage \
                 layer are sized for a specific dim, swapping would produce garbage \
                 similarity scores)"
            ))));
        }
        self.embedder = maybe_cached_embedder(embedder);
        Ok(self)
    }

    /// Escape hatch — replace the reranker on an existing handle.
    ///
    /// [`Lunaris::open`] constructs the default `NativeReranker` backed by
    /// bge-reranker-v2-m3 (v0.4 N-03 cutover) and falls back to
    /// [`NoopReranker`] on cache miss per the RETRIEVE-06 contract. Tests
    /// pass `Arc::new(NoopReranker)` for determinism; production callers can
    /// wire a custom cross-encoder (e.g., a remote rerank service) without
    /// touching the rest of the construction path. Per RETRIEVE-06 this is
    /// also how callers turn the rerank pass off entirely if the per-batch
    /// budget busts on their hardware:
    /// `handle.with_reranker(Arc::new(NoopReranker))`.
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

    /// Liveness probe for `lunaris-server`'s `/healthz` rollout-cutback surface
    /// (`observability-rollout-maturity`): delegates to the storage backend's
    /// [`StoragePort::health_check`] (Moon issues a real PING; in-process
    /// backends report healthy via the additive default). `Err` → the server
    /// answers 503 so the 5%→100% rollout controller cuts traffic back.
    pub async fn health_check(&self) -> Result<(), LunarisError> {
        self.storage.health_check().await.map_err(LunarisError::Storage)
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

    /// Cross-scope enumeration — pass-through to
    /// [`StoragePort::list_scopes`](lunaris_core::StoragePort::list_scopes).
    ///
    /// Returns a paginated [`ScopePage`](lunaris_core::ScopePage) of scopes
    /// known to the underlying backend, optionally filtered by `prefix`. The
    /// cursor is opaque (Q-U1 lock) and MUST be passed back unchanged on
    /// subsequent calls; `next_cursor == None` means enumeration is exhausted.
    ///
    /// ## Backend support
    ///
    /// - **Moon** + **embedded SQLite** — supported (lazy SCAN-parse derivation
    ///   from the `lunaris:{scope}:…` keyspace).
    /// - **Postgres** — returns `Err(StorageError::NotSupported(_))`. The
    ///   primitive tables are RLS-protected with `FORCE ROW LEVEL SECURITY`
    ///   (migration `20260510000005_scope_partitioning.sql`) and the
    ///   application role cannot bypass it. Callers MUST handle `NotSupported`
    ///   by either (a) supplying a known scope list from caller context, or
    ///   (b) routing the enumeration through a Moon-fronted instance.
    ///
    /// This is the v0.3 surface introduced by the cross-scope enumeration
    /// patch. The higher-level `list_atoms` / `get_atom_by_scope_lsn` from the
    /// upstream brief are intentionally deferred — Lunaris exposes six
    /// primitive kinds (episode/chunk/entity/relation/fact/community) rather
    /// than a unified `Atom`, and introducing that abstraction is a separate
    /// design pass.
    ///
    /// ## Example
    ///
    /// ```ignore
    /// use lunaris::Lunaris;
    /// let engine = Lunaris::open("memory://").await?;
    /// let page = engine.list_scopes(None, 100, None).await?;
    /// for scope in page.scopes {
    ///     println!("known scope: {scope:?}");
    /// }
    /// # Ok::<(), lunaris::LunarisError>(())
    /// ```
    pub async fn list_scopes(
        &self,
        prefix: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<lunaris_core::ScopePage, LunarisError> {
        self.storage.list_scopes(prefix, limit, cursor).await.map_err(LunarisError::from)
    }

    /// Bulk-invalidate FT index records authored by `node_id` within the HLC wall-clock
    /// window `[hlc_wall_lo_inclusive, hlc_wall_hi_inclusive]` (both ends inclusive).
    ///
    /// Called by Helios when `helios-git` detects a force-push or rebase that abandons
    /// commits. This evicts stale recall from the agent's memory so subsequent queries
    /// do not surface facts from the abandoned branch.
    ///
    /// ## Fan-out
    ///
    /// The method issues `FT.INVALIDATE_RANGE` against each known Lunaris collection
    /// (`chunks`, `entities`, `facts`, `communities`) in parallel via `join_all`.
    /// Collections whose index is missing on Moon (`WRONGTYPE` response) or whose
    /// backend does not support the primitive (`NotSupported`) are skipped with a
    /// `WARN` log (degraded mode — the caller receives a partial count, not an error).
    ///
    /// ## HLC wall-clock semantics
    ///
    /// `hlc_wall_lo_inclusive` and `hlc_wall_hi_inclusive` are milliseconds since
    /// the Unix epoch, matching Moon's `hlc_wall` NUMERIC field convention. Both
    /// bounds are **inclusive** (Moon `[lo, hi]` closed interval). Callers with a
    /// half-open Rust range `lo..hi` must pass `hi - 1` as the upper bound.
    ///
    /// ## Timeout
    ///
    /// Each per-index call is bounded to 250 ms (CONTEXT.md §5 IO failure surface).
    /// There is no retry — this is a bulk admin operation; the caller decides retry
    /// policy.
    ///
    /// ## Schema preconditions
    ///
    /// For the invalidation to match documents, the target FT indices must declare:
    /// - `hlc_node_id` as a `TAG` field
    /// - `hlc_wall` as a `NUMERIC` field
    ///
    /// Indices lacking these fields return 0 silently (Moon bitmap intersect returns
    /// empty). This is expected for indices predating the `helios-git` schema additions;
    /// see `.planning/W2-L2-INVALIDATE-RANGE-SUMMARY.md` for the full schema roadmap.
    ///
    /// ## Empty range
    ///
    /// If `hlc_wall_lo_inclusive > hlc_wall_hi_inclusive`, the method returns `Ok(0)`
    /// immediately without issuing any wire calls.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Helios force-push detector hands us the abandoned HLC window:
    /// use lunaris_core::Scope;
    /// let scope = Scope::new("helios:my-worktree").unwrap();
    /// let count = lunaris.invalidate_range(
    ///     &scope,
    ///     "helios-git@aabbcc",
    ///     1_700_000_000_000,
    ///     1_700_000_100_000,
    /// ).await?;
    /// tracing::info!(count, "invalidated stale recall");
    /// ```
    pub async fn invalidate_range(
        &self,
        scope: &Scope,
        node_id: &str,
        hlc_wall_lo_inclusive: i64,
        hlc_wall_hi_inclusive: i64,
    ) -> Result<u64, LunarisError> {
        crate::invalidate::invalidate_range(
            &self.storage,
            scope,
            node_id,
            hlc_wall_lo_inclusive,
            hlc_wall_hi_inclusive,
        )
        .await
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

// ── HOOK-05: idempotency ──────────────────────────────────────────────────────

/// Outcome of [`ScopedLunaris::ingest_idempotent`] (HOOK-05).
///
/// `Fresh` means a new episode was written; `Duplicate` means the dedupe key
/// was already present and the prior LSN is returned without a second
/// `atomic_write`. INGEST-04 is preserved: `Duplicate` does NOT call
/// `atomic_write` at all; `Fresh` calls it exactly once via [`ScopedLunaris::ingest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestKind {
    /// New episode was written; the enclosed `Lsn` is its committed LSN.
    Fresh,
    /// Episode already present; the enclosed `Lsn` is the prior committed LSN.
    Duplicate(lunaris_core::Lsn),
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

    /// Idempotent ingest (HOOK-05): if `dedupe_key` has been seen before within
    /// this scope, return the prior `Lsn` without a second `atomic_write`.
    ///
    /// ## INGEST-04 invariant preserved
    ///
    /// The dedupe key lookup is READ-ONLY (`StoragePort::lookup_by_dedupe_key`).
    /// Only on [`IngestKind::Fresh`] does the existing single `atomic_write`
    /// (inside [`Self::ingest`]) run. No new `atomic_write` call site is introduced.
    ///
    /// ## Trait-method approach (W6 fix)
    ///
    /// Uses `StoragePort::lookup_by_dedupe_key` / `insert_dedupe_key` trait methods
    /// directly — no `as_any()` downcast. Moon and Postgres return `Ok(None)` from
    /// the trait default, causing a safe fall-through to unconditional Fresh ingest
    /// on those backends (documented v0.5 scope boundary: SQLite-only idempotency).
    ///
    /// ## Post-commit race window (T-24-03-06)
    ///
    /// `insert_dedupe_key` runs AFTER the `atomic_write` commit. If the process is
    /// killed in the window between those two operations, replay produces a duplicate
    /// Episode. Mitigation deferred to v0.6. The `insert_dedupe_key` failure is
    /// non-fatal (logged at WARN level).
    pub async fn ingest_idempotent(
        &self,
        builder: EpisodeBuilder,
        dedupe_key: &str,
    ) -> Result<(Lsn, IngestKind), LunarisError> {
        // Attempt read-only lookup via StoragePort trait method.
        // EmbeddedStorage returns the real hit; Moon/Postgres return Ok(None).
        match self.engine.storage.lookup_by_dedupe_key(&self.scope, dedupe_key).await {
            Ok(Some(prior_lsn)) => {
                tracing::debug!(
                    dedupe_key,
                    prior_lsn = %prior_lsn,
                    scope = self.scope.as_str(),
                    "duplicate dedupe key — returning prior LSN without ingest",
                );
                return Ok((prior_lsn, IngestKind::Duplicate(prior_lsn)));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    dedupe_key,
                    "dedupe key lookup failed — proceeding as fresh ingest",
                );
            }
        }

        // Fresh path: ingest (single atomic_write inside self.ingest), then
        // record the dedupe key in the sidecar table (best-effort, non-fatal).
        let lsn = self.ingest(builder).await?;

        if let Err(e) = self.engine.storage.insert_dedupe_key(&self.scope, dedupe_key, lsn).await {
            tracing::warn!(
                err = %e,
                dedupe_key,
                lsn = %lsn,
                "dedupe key insert failed — continuing (non-fatal, T-24-03-06 race window)",
            );
        }

        Ok((lsn, IngestKind::Fresh))
    }

    /// Phase 23 — agent-supplied structured ingest under the bound scope.
    ///
    /// Delegates to [`Lunaris::ingest_structured`] with `self.scope` so
    /// the caller cannot inject an arbitrary scope. See the
    /// [`crate::structured_ingest`] module rustdoc for the full design
    /// (deterministic EntityId, always-on graph writes, single
    /// `atomic_write` per call).
    ///
    /// INGEST-04 invariant: exactly one `atomic_write` call per ingest
    /// path. The write lives in
    /// [`crate::structured_ingest::ingest_structured_inner`] for this path
    /// (vs. `lunaris_ingest::ingest_episode` / `ingest_episode_graph_on`
    /// for [`Self::ingest`]).
    pub async fn ingest_structured(
        &self,
        payload: crate::structured_ingest::StructuredIngest,
    ) -> Result<Lsn, LunarisError> {
        self.engine.ingest_structured(payload, self.scope.clone()).await
    }

    /// Recall hits under the bound scope.
    ///
    /// Runs the **default plan** (a `Vector` search over the `chunks` index —
    /// no keyword fusion, no rerank), executes it, and returns the hydrated
    /// `Vec<Hit>`. This is the one-shot convenience form; for a custom plan
    /// (hybrid fusion, graph / tree, `as_of`, rerank) use [`Self::dsl`].
    /// Wave 2.5C: the scope is applied to the `Vector` search and to hydrate,
    /// so only hits from this scope's partition are returned. (The same scope
    /// threading covers `Graph` / `Keyword` and any other operators you attach
    /// via [`Self::dsl`].)
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
            apply_reflect_boost(&self.engine.boost_cache, &self.scope, &output.boost, BOOST_DELTA);
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

// ── v0.4 N-03 cutover: native embedder + reranker resolution ──────────────────
//
// `LUNARIS_EMBEDDER_BACKEND` / `LUNARIS_RERANKER_BACKEND` are retired (RFC-style
// breaking change for v0.4). The supported runtime is candle-native + the
// frozen pair `granite-embedding-311m-multilingual-r2` (embedder) and
// `bge-reranker-v2-m3` (reranker). The only knobs are:
//
// - `LUNARIS_EMBEDDER_DIR` — optional override for the granite-r2 model
//   directory (default `~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/`).
// - `LUNARIS_RERANKER_DIR` — optional override for the bge-reranker dir
//   (default `~/.cache/lunaris/models/bge-reranker-v2-m3/`).
// - `LUNARIS_EMBEDDER_GGUF` — optional path to a Q4_K_M GGUF; activates the
//   quantized embedder ONLY when the `embedder-gguf` feature is enabled.
// - `LUNARIS_RERANKER_GGUF` — same for the quantized reranker (`reranker-gguf`).
// - `LUNARIS_EMBEDDER_OLLAMA_URL` — operator escape hatch; only consulted when
//   the `embed-remote` feature is enabled. NOT the supported path.
// - `LUNARIS_EMBED_DIM` — only applies when the resolver falls back to
//   `NoopEmbedder` (granite-r2 weights missing); default 768.
//
// Cache layout:
//   ~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/
//     ├── model.safetensors
//     ├── tokenizer.json
//     └── config.json
//   ~/.cache/lunaris/models/bge-reranker-v2-m3/
//     ├── model.safetensors
//     ├── tokenizer.json
//     └── config.json
//
// One-shot tracing::info! per process logs the resolved backend + path; if
// the embedder falls back to noop the operator gets a tracing::warn! banner.

/// Optional override for the directory holding the granite-r2 model
/// artifacts. Default: `<cache-dir>/lunaris/models/granite-embedding-311m-multilingual-r2/`.
/// Expected layout: `model.safetensors`, `tokenizer.json`, `config.json`.
pub const EMBEDDER_DIR_ENV_VAR: &str = "LUNARIS_EMBEDDER_DIR";

/// Optional override for the directory holding the bge-reranker-v2-m3 model
/// artifacts. Default: `<cache-dir>/lunaris/models/bge-reranker-v2-m3/`.
pub const RERANKER_DIR_ENV_VAR: &str = "LUNARIS_RERANKER_DIR";

/// Optional path to a Q4_K_M GGUF for the embedder. Only consulted when the
/// `embedder-gguf` feature is enabled. When set, activates
/// `NativeQuantizedEmbedder` instead of the default FP16
/// `NativeEmbedder`.
pub const EMBEDDER_GGUF_ENV_VAR: &str = "LUNARIS_EMBEDDER_GGUF";

/// Optional path to a Q4_K_M GGUF for the reranker. Only consulted when the
/// `reranker-gguf` feature is enabled.
pub const RERANKER_GGUF_ENV_VAR: &str = "LUNARIS_RERANKER_GGUF";

/// Env var that controls the dim of the `NoopEmbedder` fallback used when
/// the granite-r2 weights are missing AND the operator has not supplied a
/// custom embedder via [`Lunaris::with_embedder`]. Positive integer; default
/// [`lunaris_core::NOOP_DEFAULT_DIM`] (768).
pub const EMBED_DIM_ENV_VAR: &str = "LUNARIS_EMBED_DIM";

/// Env var that controls the maximum number of concurrent speculative warm-up
/// recall tasks spawned by [`ScopedLunaris::end_turn`] (Phase 14.3).
/// Must be a positive integer. `0`, non-numeric, or unset values fall back to
/// the default of `4`. One `tracing::info!` is emitted per process when the
/// capacity is resolved.
pub const PREWARM_CONCURRENCY_ENV_VAR: &str = "LUNARIS_PREWARM_CONCURRENCY";

/// Default semaphore capacity for speculative warm-up recalls.
const PREWARM_CONCURRENCY_DEFAULT: usize = 4;

/// Env var that controls the exact-text embedding cache capacity.
///
/// Set to `0` to disable the cache. The default is intentionally modest:
/// enough for repeated agent prompts, context-injection recalls, and common
/// chunk text, but bounded so long-running agents do not grow without limit.
pub const EMBED_CACHE_CAPACITY_ENV_VAR: &str = "LUNARIS_EMBED_CACHE_CAPACITY";

const EMBED_CACHE_CAPACITY_DEFAULT: usize = 2048;

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

fn embed_cache_capacity() -> Option<NonZeroUsize> {
    let capacity = match std::env::var(EMBED_CACHE_CAPACITY_ENV_VAR).ok().as_deref() {
        None | Some("") => EMBED_CACHE_CAPACITY_DEFAULT,
        Some("0") => return None,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => return None,
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    env = EMBED_CACHE_CAPACITY_ENV_VAR,
                    value = raw,
                    default = EMBED_CACHE_CAPACITY_DEFAULT,
                    "LUNARIS_EMBED_CACHE_CAPACITY is not a valid non-negative integer; using default"
                );
                EMBED_CACHE_CAPACITY_DEFAULT
            }
        },
    };
    NonZeroUsize::new(capacity)
}

fn maybe_cached_embedder(embedder: Arc<dyn Embedder>) -> Arc<dyn Embedder> {
    match embed_cache_capacity() {
        Some(capacity) => Arc::new(CachedEmbedder::new(embedder, capacity)) as Arc<dyn Embedder>,
        None => embedder,
    }
}

static EMBEDDER_BACKEND_LOG_ONCE: OnceLock<()> = OnceLock::new();
static RERANKER_BACKEND_LOG_ONCE: OnceLock<()> = OnceLock::new();

/// Granite-r2 model directory name under `<cache>/lunaris/models/`.
const GRANITE_R2_DIR: &str = "granite-embedding-311m-multilingual-r2";
/// bge-reranker-v2-m3 model directory name under `<cache>/lunaris/models/`.
/// Referenced by the `native`-gated reranker path (and the dir-layout unit test).
#[cfg(any(feature = "native", test))]
const BGE_RERANKER_DIR: &str = "bge-reranker-v2-m3";

/// Resolve the canonical cache directory for a named model artifact. Returns
/// `<cache_dir>/lunaris/models/<name>/`, or `./lunaris/models/<name>/` when
/// `dirs::cache_dir()` is unavailable (rare on Unix/macOS — surfaced as a
/// warning to operators of stripped-down environments).
fn default_model_dir(name: &str) -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("lunaris")
        .join("models")
        .join(name)
}

/// Resolve the embedder model directory from [`EMBEDDER_DIR_ENV_VAR`],
/// falling back to the default cache layout.
fn embedder_dir() -> std::path::PathBuf {
    std::env::var(EMBEDDER_DIR_ENV_VAR)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default_model_dir(GRANITE_R2_DIR))
}

/// Resolve the reranker model directory from [`RERANKER_DIR_ENV_VAR`],
/// falling back to the default cache layout. Only referenced by the
/// `native`-gated reranker path.
#[cfg(feature = "native")]
fn reranker_dir() -> std::path::PathBuf {
    std::env::var(RERANKER_DIR_ENV_VAR)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default_model_dir(BGE_RERANKER_DIR))
}

/// v0.4 N-03 — resolve the default embedder for [`Lunaris::open`]. Tries:
///
/// 1. (feature `embed-remote`) If `LUNARIS_EMBEDDER_OLLAMA_URL` is set,
///    construct [`lunaris_embed_remote::OllamaEmbedder`]. Operator escape
///    hatch; emits a runtime warn.
/// 2. (feature `embedder-gguf`) If `LUNARIS_EMBEDDER_GGUF` is set, construct
///    `NativeQuantizedEmbedder` from the GGUF path + the granite-r2 tokenizer
///    found via `embedder_dir()`.
/// 3. Otherwise, construct [`lunaris_embed_native::NativeEmbedder`] from
///    `<embedder_dir>/model.safetensors` + `tokenizer.json` + `config.json`.
/// 4. On cache miss, emit a `tracing::warn!` and fall back to
///    [`NoopEmbedder`] at [`lunaris_core::NOOP_DEFAULT_DIM`] so the rest
///    of the open path completes (vector recall returns empty rows; operator
///    sees the banner and can fix their cache layout).
async fn resolve_embedder() -> Result<Arc<dyn Embedder>, LunarisError> {
    // 1. Remote OpenAI-compatible embedder (`POST /v1/embeddings`) — the
    //    supported remote path once `native` (candle) is compiled out. Selected
    //    when LUNARIS_EMBEDDER_OPENAI_URL is set; wins over the Ollama hatch.
    #[cfg(feature = "embed-remote")]
    {
        if std::env::var(lunaris_embed_remote::openai::OPENAI_URL_ENV_VAR)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
        {
            let opts = lunaris_embed_remote::openai::OpenAiEmbedderOpts::default();
            let e = lunaris_embed_remote::openai::OpenAiEmbedder::new(opts)?;
            EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
                tracing::info!(
                    target: "lunaris::handle",
                    embedder_backend = "openai-remote",
                    "embedder_backend_resolved (remote OpenAI-compatible /embeddings)"
                );
            });
            return Ok(Arc::new(e) as Arc<dyn Embedder>);
        }
    }

    // 1b. Ollama HTTP escape hatch — legacy remote path.
    #[cfg(feature = "embed-remote")]
    {
        if let Some(url) =
            std::env::var(lunaris_embed_remote::OLLAMA_URL_ENV_VAR).ok().filter(|s| !s.is_empty())
        {
            let opts =
                lunaris_embed_remote::OllamaEmbedderOpts { endpoint: url, ..Default::default() };
            let e = lunaris_embed_remote::OllamaEmbedder::new(opts)?;
            EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
                tracing::info!(
                    target: "lunaris::handle",
                    embedder_backend = "ollama-remote",
                    "embedder_backend_resolved (operator escape hatch)"
                );
            });
            return Ok(Arc::new(e) as Arc<dyn Embedder>);
        }
    }

    // 2. Quantized GGUF — only when feature is on AND env var is set.
    #[cfg(feature = "embedder-gguf")]
    {
        if let Some(gguf_path) = std::env::var(EMBEDDER_GGUF_ENV_VAR).ok().filter(|s| !s.is_empty())
        {
            let dir = embedder_dir();
            let device = candle_core::Device::Cpu;
            let opts = lunaris_embed_native::NativeQuantizedEmbedderOpts {
                gguf_path: std::path::PathBuf::from(&gguf_path),
                tokenizer_path: dir.join("tokenizer.json"),
                config_path: dir.join("config.json"),
                device,
            };
            match lunaris_embed_native::NativeQuantizedEmbedder::open(opts) {
                Ok(e) => {
                    EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
                        tracing::info!(
                            target: "lunaris::handle",
                            embedder_backend = "native-quantized",
                            gguf = %gguf_path,
                            "embedder_backend_resolved"
                        );
                    });
                    return Ok(Arc::new(e) as Arc<dyn Embedder>);
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        gguf = %gguf_path,
                        "LUNARIS_EMBEDDER_GGUF set but quantized embedder failed to open; \
                         falling through to FP16"
                    );
                }
            }
        }
    }

    // 3. Default FP16 native (candle) embedder — only when the `native` feature
    //    is compiled in. All candle / lunaris_embed_native references live in
    //    the gated helper below so a `default-features = false` build is
    //    candle-free.
    #[cfg(feature = "native")]
    {
        resolve_embedder_native().await
    }

    // 4. Candle disabled — NoopEmbedder (zero vectors). Vector recall returns
    //    empty rows until a remote embedder is configured. Build with
    //    `--features embed-remote` and set LUNARIS_EMBEDDER_OLLAMA_URL for a
    //    remote server embedder.
    #[cfg(not(feature = "native"))]
    {
        let dim = resolve_embed_dim();
        EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
            tracing::warn!(
                target: "lunaris::handle",
                fallback_dim = dim,
                "native embedder feature disabled — using NoopEmbedder (zero vectors). \
                 Configure a remote embedder (build with --features embed-remote and set \
                 LUNARIS_EMBEDDER_OLLAMA_URL) for real vectors."
            );
        });
        Ok(Arc::new(lunaris_core::NoopEmbedder::new(dim)) as Arc<dyn Embedder>)
    }
}

/// Step 3 of [`resolve_embedder`] — the candle FP16 `NativeEmbedder` path.
/// Extracted behind `#[cfg(feature = "native")]` so every `candle_core` /
/// `lunaris_embed_native` reference is elided from candle-free builds.
#[cfg(feature = "native")]
async fn resolve_embedder_native() -> Result<Arc<dyn Embedder>, LunarisError> {
    let dir = embedder_dir();
    let opts = lunaris_embed_native::NativeEmbedderOpts {
        weights_path: dir.join("model.safetensors"),
        tokenizer_path: dir.join("tokenizer.json"),
        config_path: dir.join("config.json"),
        device: candle_core::Device::Cpu,
    };
    match lunaris_embed_native::NativeEmbedder::open(opts) {
        Ok(e) => {
            EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
                tracing::info!(
                    target: "lunaris::handle",
                    embedder_backend = "native",
                    weights_dir = %dir.display(),
                    "embedder_backend_resolved"
                );
            });
            Ok(Arc::new(e) as Arc<dyn Embedder>)
        }
        Err(err) => {
            // Cache miss — NoopEmbedder fallback so the rest of open()
            // completes. Operator sees one banner per process.
            let dim = resolve_embed_dim();
            EMBEDDER_BACKEND_LOG_ONCE.get_or_init(|| {
                tracing::warn!(
                    target: "lunaris::handle",
                    error = %err,
                    weights_dir = %dir.display(),
                    fallback_dim = dim,
                    "granite-r2 weights unavailable at the resolved model dir; falling back \
                     to NoopEmbedder (zero vectors). Vector recall will return empty rows \
                     until weights are staged. Install via \
                     `huggingface-cli download ibm-granite/granite-embedding-311m-multilingual-r2 \
                      --local-dir <weights_dir>` or override with LUNARIS_EMBEDDER_DIR=<dir>."
                );
            });
            Ok(Arc::new(lunaris_core::NoopEmbedder::new(dim)) as Arc<dyn Embedder>)
        }
    }
}

/// Resolve the NoopEmbedder fallback dim from [`EMBED_DIM_ENV_VAR`].
fn resolve_embed_dim() -> usize {
    static LOG_ONCE: OnceLock<()> = OnceLock::new();
    let dim = match std::env::var(EMBED_DIM_ENV_VAR).ok().as_deref() {
        None | Some("") => lunaris_core::NOOP_DEFAULT_DIM,
        Some(s) => match s.trim().parse::<usize>() {
            Ok(0) => {
                tracing::warn!(
                    env = EMBED_DIM_ENV_VAR,
                    value = s,
                    default = lunaris_core::NOOP_DEFAULT_DIM,
                    "LUNARIS_EMBED_DIM=0 is invalid (storage rejects dim=0); using default"
                );
                lunaris_core::NOOP_DEFAULT_DIM
            }
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    env = EMBED_DIM_ENV_VAR,
                    value = s,
                    default = lunaris_core::NOOP_DEFAULT_DIM,
                    "LUNARIS_EMBED_DIM is not a valid positive integer; using default"
                );
                lunaris_core::NOOP_DEFAULT_DIM
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

/// v0.4 N-03 — resolve the default reranker for [`Lunaris::open`]. Tries:
///
/// 1. (feature `reranker-gguf`) If `LUNARIS_RERANKER_GGUF` is set, construct
///    `NativeQuantizedReranker` from the GGUF path + the bge-reranker
///    tokenizer found via `reranker_dir()`.
/// 2. Otherwise, construct [`lunaris_rerank_native::NativeReranker`] from
///    `<reranker_dir>/model.safetensors` + `tokenizer.json` + `config.json`.
/// 3. On cache miss, emit a `tracing::warn!` and fall back to
///    [`NoopReranker`] per the RETRIEVE-06 contract — the recall path runs
///    end-to-end even without the cross-encoder pass.
async fn resolve_reranker() -> Result<Arc<dyn Reranker>, LunarisError> {
    // 1. Quantized GGUF — only when feature is on.
    //
    // N-04 D1 — DO NOT call `NativeQuantizedReranker::open` here. The Q5_K_M
    // GGUF (446 MiB) mmaps into RSS the moment `open` runs, and the recall
    // hot path may never actually invoke the rerank stage (RETRIEVE-06
    // budget bust → skip). We pre-flight that the artifact paths exist (so
    // a typo'd env var still falls through to FP32 immediately, not at
    // first-rerank-time), then hand back a `LazyQuantizedReranker` which
    // defers the mmap until the first `rerank()` call.
    #[cfg(feature = "reranker-gguf")]
    {
        if let Some(gguf_path) = std::env::var(RERANKER_GGUF_ENV_VAR).ok().filter(|s| !s.is_empty())
        {
            let dir = reranker_dir();
            let opts = lunaris_rerank_native::NativeQuantizedRerankerOpts {
                gguf_path: std::path::PathBuf::from(&gguf_path),
                tokenizer_path: dir.join("tokenizer.json"),
                config_path: dir.join("config.json"),
                device: candle_core::Device::Cpu,
            };
            // Pre-flight: refuse the lazy path early if any required artifact
            // is missing. This preserves the v0.4 N-03 fall-through-to-FP32
            // behaviour on cache-miss while keeping the GGUF mmap deferred.
            let preflight_ok = opts.gguf_path.exists()
                && opts.tokenizer_path.exists()
                && opts.config_path.exists();
            if preflight_ok {
                let lazy = LazyQuantizedReranker::new(opts);
                RERANKER_BACKEND_LOG_ONCE.get_or_init(|| {
                    tracing::info!(
                        target: "lunaris::handle",
                        reranker_backend = "native-quantized (lazy)",
                        gguf = %gguf_path,
                        "reranker_backend_resolved (load deferred to first rerank())"
                    );
                });
                return Ok(Arc::new(lazy) as Arc<dyn Reranker>);
            } else {
                tracing::warn!(
                    gguf = %gguf_path,
                    tokenizer = %opts.tokenizer_path.display(),
                    config = %opts.config_path.display(),
                    "LUNARIS_RERANKER_GGUF set but one or more artifacts are missing on disk; \
                     falling through to FP32"
                );
            }
        }
    }

    // 2. Default FP32 native (candle) reranker — only when the `native` feature
    //    is compiled in. Gated helper keeps candle out of candle-free builds.
    #[cfg(feature = "native")]
    {
        resolve_reranker_native().await
    }

    // 3. Candle disabled — NoopReranker (rerank pass skipped per RETRIEVE-06).
    #[cfg(not(feature = "native"))]
    {
        RERANKER_BACKEND_LOG_ONCE.get_or_init(|| {
            tracing::info!(
                target: "lunaris::handle",
                reranker_backend = "noop",
                "native reranker feature disabled — using NoopReranker (rerank pass skipped \
                 per RETRIEVE-06 contract)."
            );
        });
        Ok(Arc::new(NoopReranker) as Arc<dyn Reranker>)
    }
}

/// Step 2 of [`resolve_reranker`] — the candle FP32 `NativeReranker` path.
/// Extracted behind `#[cfg(feature = "native")]` so every `candle_core` /
/// `lunaris_rerank_native` reference is elided from candle-free builds.
#[cfg(feature = "native")]
async fn resolve_reranker_native() -> Result<Arc<dyn Reranker>, LunarisError> {
    let dir = reranker_dir();
    let opts = lunaris_rerank_native::NativeRerankerOpts {
        weights_path: dir.join("model.safetensors"),
        tokenizer_path: dir.join("tokenizer.json"),
        config_path: dir.join("config.json"),
        device: candle_core::Device::Cpu,
    };
    match lunaris_rerank_native::NativeReranker::open(opts) {
        Ok(r) => {
            RERANKER_BACKEND_LOG_ONCE.get_or_init(|| {
                tracing::info!(
                    target: "lunaris::handle",
                    reranker_backend = "native",
                    weights_dir = %dir.display(),
                    "reranker_backend_resolved"
                );
            });
            Ok(Arc::new(r) as Arc<dyn Reranker>)
        }
        // Cache miss — NoopReranker fallback per RETRIEVE-06.
        Err(err) => {
            RERANKER_BACKEND_LOG_ONCE.get_or_init(|| {
                tracing::warn!(
                    target: "lunaris::handle",
                    error = %err,
                    weights_dir = %dir.display(),
                    "bge-reranker-v2-m3 unavailable at the resolved model dir; falling back \
                     to NoopReranker (recall budget skips the rerank pass per RETRIEVE-06 \
                     contract). Install via \
                     `huggingface-cli download BAAI/bge-reranker-v2-m3 --local-dir <weights_dir>` \
                     or override with LUNARIS_RERANKER_DIR=<dir>."
                );
            });
            Ok(Arc::new(NoopReranker) as Arc<dyn Reranker>)
        }
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
/// **`extractor-gguf` (workstream A, see
/// `docs/design/quantized-inference-extractor-reranker.md`):** unlike the
/// embedder/reranker, the Q4 GGUF ladder is NOT resolved here — it lives
/// inside `CandleGemma3_4B::new()` itself
/// (`lunaris_extract::candle_gemma3::resolve_backend`), which this function
/// already calls unconditionally. When the `extractor-gguf` feature is on
/// AND `LUNARIS_EXTRACTOR_GGUF` is set, that inner call resolves
/// `lunaris_llm::QuantizedCandleBackend` first, falling back to the F32
/// path (and, from here, to [`NoopExtractor`] on total failure) with no
/// change needed in this function.
///
/// Callers wire their own extractor via [`Lunaris::with_extractor`] or
/// `handle.graph_pipeline().set_extractor(extractor)` for late binding.
#[cfg(feature = "candle")]
async fn default_extractor() -> Arc<dyn Extractor> {
    match lunaris_extract::CandleGemma3_4B::new(Default::default()).await {
        // Wrap the real extractor with the production fallback floor: a transient
        // primary failure degrades to NoopExtractor (graph extraction off for that
        // episode) instead of failing ingest; terminal errors still propagate.
        // This is what puts FallbackExtractor + CircuitBreaker on the production
        // open() path (io-failsafe-wiring Half A).
        Ok(e) => lunaris_extract::fallback::fallback_wrap(e, "gemma-3-4b-it"),
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

// ── N-04 D1 — lazy quantized reranker ────────────────────────────────────────
//
// `NativeQuantizedReranker::open` mmaps the 446 MiB Q5_K_M-imatrix GGUF the
// moment it is called. Eagerly calling it inside `resolve_reranker()`
// inflates `Lunaris::open()` RSS by ~440 MiB even when the recall hot path
// never reaches the rerank stage (e.g., budget-bust per RETRIEVE-06, or
// callers that override the reranker before issuing recall).
//
// `LazyQuantizedReranker` defers the mmap until the first `rerank()` call
// via `tokio::sync::OnceCell::get_or_try_init`. The trait `applies()`
// answer stays `true` (config promises a real reranker — the
// `rerank_applied` flag on `Hit` must reflect intent, not init state)
// matching `NativeQuantizedReranker::applies` verbatim.
//
// On lazy-init failure the error is surfaced to the caller via
// `LunarisError::Storage(Backend(_))` — the OnceCell is left empty so a
// later call can retry (e.g., operator fixes a permissions issue between
// rerank attempts). The init closure runs inside `spawn_blocking` so the
// 446 MiB mmap + tensor parse doesn't stall the tokio runtime.
#[cfg(feature = "reranker-gguf")]
struct LazyQuantizedReranker {
    opts: lunaris_rerank_native::NativeQuantizedRerankerOpts,
    cell: tokio::sync::OnceCell<Arc<lunaris_rerank_native::NativeQuantizedReranker>>,
}

#[cfg(feature = "reranker-gguf")]
impl LazyQuantizedReranker {
    fn new(opts: lunaris_rerank_native::NativeQuantizedRerankerOpts) -> Self {
        Self { opts, cell: tokio::sync::OnceCell::new() }
    }

    /// First-call init under `get_or_try_init`. Hand-rolls the `Result`
    /// shape required by OnceCell instead of `match`-and-rewrap because
    /// the error type leaving this fn MUST be `LunarisError` (OnceCell's
    /// stored type is the success arm only).
    async fn get_or_load(
        &self,
    ) -> Result<Arc<lunaris_rerank_native::NativeQuantizedReranker>, LunarisError> {
        let opts = self.opts.clone();
        self.cell
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || {
                    lunaris_rerank_native::NativeQuantizedReranker::open(opts)
                })
                .await
                .map_err(|e| {
                    LunarisError::Storage(lunaris_core::StorageError::Backend(format!(
                        "lazy reranker init join: {e}"
                    )))
                })?
                .map(Arc::new)
                .map_err(LunarisError::from)
            })
            .await
            .cloned()
    }
}

#[cfg(feature = "reranker-gguf")]
impl std::fmt::Debug for LazyQuantizedReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyQuantizedReranker")
            .field("gguf", &self.opts.gguf_path)
            .field("loaded", &self.cell.initialized())
            .finish()
    }
}

#[cfg(feature = "reranker-gguf")]
#[async_trait::async_trait]
impl Reranker for LazyQuantizedReranker {
    fn applies(&self) -> bool {
        // Config promises a real reranker — answer eagerly so `Hit { rerank_applied }`
        // doesn't lie on cold paths.
        true
    }

    async fn rerank(
        &self,
        query: &str,
        docs: Vec<lunaris_rerank::RerankCandidate>,
    ) -> Result<Vec<lunaris_rerank::RerankCandidate>, LunarisError> {
        let inner = self.get_or_load().await?;
        inner.rerank(query, docs).await
    }
}

// ── v0.4 N-03 — unit tests for env-var resolution (cache-dir layout) ─────────
//
// `resolve_embedder()` / `resolve_reranker()` are async + perform I/O; the
// unit tests below cover only the pure path-resolution helpers and the
// `resolve_embed_dim()` parser, which are deterministic and side-effect-free.
// Construction of real `NativeEmbedder` / `NativeReranker` is exercised by
// the native crates' own `numerical_equivalence` integration tests.
#[cfg(test)]
mod backend_resolution_tests {
    use super::*;
    use lunaris_core::StubEmbedder;

    struct CountingEmbedder {
        inner: StubEmbedder,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Embedder for CountingEmbedder {
        fn dim(&self) -> usize {
            self.inner.dim()
        }

        async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.embed_batch(inputs).await
        }
    }

    #[test]
    fn default_model_dir_layout_is_canonical() {
        let p = default_model_dir(GRANITE_R2_DIR);
        assert!(
            p.ends_with("lunaris/models/granite-embedding-311m-multilingual-r2"),
            "default granite-r2 dir was: {}",
            p.display()
        );
        let p = default_model_dir(BGE_RERANKER_DIR);
        assert!(
            p.ends_with("lunaris/models/bge-reranker-v2-m3"),
            "default bge dir was: {}",
            p.display()
        );
    }

    #[test]
    fn env_var_constants_are_grep_pinned() {
        // Pin the v0.4 env-var surface area so accidental renames surface in
        // review. Operators wire these strings into Helm charts / k8s
        // manifests; renaming silently breaks deployments.
        assert_eq!(EMBEDDER_DIR_ENV_VAR, "LUNARIS_EMBEDDER_DIR");
        assert_eq!(RERANKER_DIR_ENV_VAR, "LUNARIS_RERANKER_DIR");
        assert_eq!(EMBEDDER_GGUF_ENV_VAR, "LUNARIS_EMBEDDER_GGUF");
        assert_eq!(RERANKER_GGUF_ENV_VAR, "LUNARIS_RERANKER_GGUF");
        assert_eq!(EMBED_DIM_ENV_VAR, "LUNARIS_EMBED_DIM");
    }

    #[tokio::test]
    async fn cached_embedder_dedupes_batch_and_reuses_later_hits() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingEmbedder { inner: StubEmbedder::new(8), calls: calls.clone() })
            as Arc<dyn Embedder>;
        let cached = CachedEmbedder::new(inner, NonZeroUsize::new(8).unwrap());

        let first = cached.embed_batch(&["alpha", "alpha", "beta"]).await.unwrap();
        assert_eq!(first.len(), 3);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "first batch should dedupe misses");
        assert_eq!(first[0], first[1]);

        let second = cached.embed_batch(&["beta", "alpha"]).await.unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "second batch should be served entirely from cache"
        );
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
                graph_decay_native: false,
                graph_navigate_native: false,
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
