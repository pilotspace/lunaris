//! Phase 22 — operator-facing no-op embedder.
//!
//! `NoopEmbedder` returns all-zero vectors of a caller-configured `dim`. It
//! exists so a Lunaris build with **zero embedder backend features compiled
//! in** (`cargo build -p lunaris-memory --no-default-features`) still has a
//! working `Lunaris::open()` path — vector recall returns "no signal" rather
//! than crashing the storage layer.
//!
//! ## Semantics
//!
//! - `embed_batch(inputs)` returns `inputs.len()` zero vectors of length `dim`.
//! - `dim()` returns the value provided at construction.
//! - The trait impl is `async` to match [`Embedder`] but does no work
//!   off-thread — it allocates the result vector synchronously inside the
//!   future and yields immediately.
//!
//! ## When to use
//!
//! - Air-gapped operators who want metadata-only ingest and bring-your-own
//!   vectors later via [`Lunaris::with_embedder`](https://docs.rs/lunaris-memory).
//! - Build matrix CI jobs that don't want the ORT/HF-Hub transitive deps.
//! - Tests that need a real `Arc<dyn Embedder>` but don't care about vector
//!   semantics. The [`lunaris_core::StubEmbedder`] still owns the
//!   *deterministic-random-vector* niche (used where similarity must be
//!   non-trivial); `NoopEmbedder` owns the *true-zero-vector* niche.
//!
//! ## Storage interaction
//!
//! Moon's `FT.CREATE` requires `dim > 0`; Postgres/sqlite vector columns are
//! sized at migration time. Therefore `NoopEmbedder::new(0)` will surface as
//! a `StorageError::Backend("vector dim must be > 0")` at
//! `Lunaris::open("moon://...")` time. The constructor itself does NOT
//! reject `0` — the storage layer is the source of truth for dim invariants,
//! and we want the same error path whether the dim arrives from
//! `NoopEmbedder`, `StubEmbedder`, or a misconfigured real backend.

use async_trait::async_trait;

use lunaris_core::{Embedder, LunarisError};

/// Default dim when nothing else specifies one. Matches
/// `lunaris_storage_moon::DEFAULT_VECTOR_DIM` so a noop-backed
/// `Lunaris::open("moon://...")` lights up at the same dim as the historical
/// EmbeddingGemma default. Operators override via `Lunaris::open_with_embed_dim`
/// or the `LUNARIS_EMBED_DIM` env var.
pub const NOOP_DEFAULT_DIM: usize = 768;

/// Zero-vector [`Embedder`] of configurable `dim`. See module docs.
#[derive(Debug, Clone, Copy)]
pub struct NoopEmbedder {
    dim: usize,
}

impl NoopEmbedder {
    /// Construct a noop embedder reporting `dim`. `0` is accepted at the
    /// constructor level — the storage backends will reject it downstream
    /// (Moon: `vector dim must be > 0`).
    #[must_use]
    pub const fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for NoopEmbedder {
    fn default() -> Self {
        Self::new(NOOP_DEFAULT_DIM)
    }
}

#[async_trait]
impl Embedder for NoopEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok((0..inputs.len()).map(|_| vec![0.0_f32; self.dim]).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_dim_is_768() {
        let e = NoopEmbedder::default();
        assert_eq!(e.dim(), NOOP_DEFAULT_DIM);
        assert_eq!(e.dim(), 768);
    }

    #[tokio::test]
    async fn embed_batch_returns_zero_vectors_at_configured_dim() {
        let e = NoopEmbedder::new(384);
        let out = e.embed_batch(&["a", "b", "c"]).await.unwrap();
        assert_eq!(out.len(), 3);
        for row in &out {
            assert_eq!(row.len(), 384);
            assert!(row.iter().all(|&v| v == 0.0));
        }
    }

    #[tokio::test]
    async fn empty_input_yields_empty_output() {
        let e = NoopEmbedder::new(768);
        let out = e.embed_batch(&[]).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn dim_zero_is_accepted_at_constructor() {
        // Storage layers reject dim=0; the constructor does not, so the
        // error path stays uniform across embedder impls.
        let e = NoopEmbedder::new(0);
        assert_eq!(e.dim(), 0);
        let out = e.embed_batch(&["x"]).await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].is_empty());
    }
}
