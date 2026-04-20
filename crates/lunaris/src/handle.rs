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

use lunaris_core::{Embedder, HlcClock, KeywordPort, LunarisError, StoragePort};
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_postgres::PostgresStorage;

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
}

impl std::fmt::Debug for Lunaris {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lunaris")
            .field("backend_capabilities", &self.storage.capabilities())
            .field("embedder_dim", &self.embedder.dim())
            .field("clock_node_id", &self.clock.node_id())
            .field("has_moon_native_path", &self.moon_storage.is_some())
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
        match scheme {
            "moon" => {
                let m = Arc::new(MoonStorage::connect(url).await?);
                Ok(Self {
                    storage: m.clone() as Arc<dyn StoragePort>,
                    keyword: m.clone() as Arc<dyn KeywordPort>,
                    embedder,
                    clock,
                    moon_storage: Some(m),
                })
            }
            "postgres" | "postgresql" => {
                let p = Arc::new(PostgresStorage::connect(url).await?);
                Ok(Self {
                    storage: p.clone() as Arc<dyn StoragePort>,
                    keyword: p as Arc<dyn KeywordPort>,
                    embedder,
                    clock,
                    moon_storage: None,
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
        Self {
            storage,
            keyword: Arc::new(NoKeywordSupport) as Arc<dyn KeywordPort>,
            embedder,
            clock,
            moon_storage: None,
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
        Self { storage, keyword, embedder, clock, moon_storage: None }
    }

    /// Public escape hatch — replace the embedder on an existing handle.
    /// Used by the Plan 02-01 latency-budget swap (candle → Ollama).
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
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
