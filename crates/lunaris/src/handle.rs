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

use lunaris_core::{Embedder, HlcClock, LunarisError, StoragePort};

#[derive(Clone)]
pub struct Lunaris {
    pub(crate) storage: Arc<dyn StoragePort>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) clock: Arc<HlcClock>,
}

impl std::fmt::Debug for Lunaris {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lunaris")
            .field("backend_capabilities", &self.storage.capabilities())
            .field("embedder_dim", &self.embedder.dim())
            .field("clock_node_id", &self.clock.node_id())
            .finish()
    }
}

impl Lunaris {
    /// Production constructor. Opens a storage backend by URL and constructs
    /// the default embedder.
    ///
    /// - `moon://...` → [`lunaris_storage_moon::MoonStorage`] backend
    /// - `postgres://...` → [`lunaris_storage_postgres::PostgresStorage`] backend
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
        let storage = crate::open::open(url).await?;
        let embedder = default_embedder().await?;
        let clock = HlcClock::new(0);
        Ok(Self { storage, embedder, clock })
    }

    /// Test / latency-budget-swap escape hatch. Wires a custom storage handle,
    /// embedder, and clock — bypasses [`Self::open`]'s default constructors.
    /// Marked `#[doc(hidden)]` because production callers should use [`Self::open`]
    /// instead, but kept on the public API so the smoke test + Phase 2 latency
    /// swap have a stable seam.
    #[doc(hidden)]
    pub fn with_parts(
        storage: Arc<dyn StoragePort>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<HlcClock>,
    ) -> Self {
        Self { storage, embedder, clock }
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
    pub fn embedder(&self) -> Arc<dyn Embedder> {
        self.embedder.clone()
    }
    pub fn clock(&self) -> Arc<HlcClock> {
        self.clock.clone()
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
