//! Embedder trait + a deterministic stub impl returning 768-dim vectors.
//!
//! Real `candle`-backed EmbeddingGemma lands in Phase 2.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use async_trait::async_trait;

use crate::error::LunarisError;

#[async_trait]
pub trait Embedder: Send + Sync + 'static {
    fn dim(&self) -> usize;
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError>;
}

#[derive(Debug, Clone)]
pub struct StubEmbedder {
    dim: usize,
}

impl StubEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|s| det_vec(s, self.dim)).collect())
    }
}

/// Always-failing [`Embedder`] for use in unit tests that must verify error
/// propagation.  Every `embed_batch` call returns
/// `LunarisError::Storage(StorageError::Backend(...))`.
///
/// Use `wrong_row_count: true` to simulate a wrong-row-count return instead
/// of a hard `Err`.
#[derive(Debug, Clone)]
pub struct FailingEmbedder {
    dim: usize,
    /// When `true`, returns `Ok` with `inputs.len() + 1` vectors instead of
    /// `Err` — exercises the row-count mismatch arm.
    pub wrong_row_count: bool,
}

impl FailingEmbedder {
    /// Construct an embedder that always errors.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self { dim, wrong_row_count: false }
    }

    /// Construct an embedder that returns the wrong row count.
    #[must_use]
    pub fn wrong_count(dim: usize) -> Self {
        Self { dim, wrong_row_count: true }
    }
}

#[async_trait::async_trait]
impl Embedder for FailingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if self.wrong_row_count {
            // Return one extra row — contract violation; callers must not zero-fill.
            Ok((0..inputs.len() + 1).map(|_| vec![0.0f32; self.dim]).collect())
        } else {
            Err(crate::error::LunarisError::Storage(crate::error::StorageError::Backend(
                "FailingEmbedder: injected failure".into(),
            )))
        }
    }
}

/// Default dim for [`NoopEmbedder`] when no override is supplied. Matches the
/// historical EmbeddingGemma 300M / granite-r2 output width (768) so an
/// operator who flips to the noop backend doesn't have to re-create their
/// `FT.CREATE` HNSW index.
///
/// Moon's `FT.CREATE` requires `dim > 0` and Postgres/sqlite vector columns
/// are sized at migration time; `NoopEmbedder::new(0)` therefore surfaces
/// as a `StorageError::Backend("vector dim must be > 0")` at
/// `Lunaris::open()` time. The constructor itself accepts `0` so the error
/// path stays uniform across `Stub` / `Noop` / real backends.
pub const NOOP_DEFAULT_DIM: usize = 768;

/// Zero-vector [`Embedder`] of caller-configured `dim`. Used when no real
/// embedder backend is wired (air-gapped builds, metadata-only ingest, tests
/// that need a working `Arc<dyn Embedder>` without similarity semantics).
///
/// `StubEmbedder` is the *deterministic-random-vector* niche; `NoopEmbedder`
/// is the *true-zero-vector* niche. Both live in `lunaris-core` so backend
/// crates can be feature-gated without losing the no-op fallback (the v0.4
/// N-03 cutover moved this out of the retired `lunaris-embed` crate into the
/// core trait module).
#[derive(Debug, Clone, Copy)]
pub struct NoopEmbedder {
    dim: usize,
}

impl NoopEmbedder {
    /// Construct a noop embedder reporting `dim`. `0` is accepted at the
    /// constructor level — the storage backends reject it downstream.
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

fn det_vec(s: &str, dim: usize) -> Vec<f32> {
    // Deterministic: seed a tiny LCG by hashing the input; emit `dim` floats in [-1, 1].
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    let mut state = h.finish().max(1);
    (0..dim)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let v = ((state >> 33) as u32) as f32 / u32::MAX as f32;
            v * 2.0 - 1.0
        })
        .collect()
}

#[cfg(test)]
mod noop_tests {
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
        let e = NoopEmbedder::new(0);
        assert_eq!(e.dim(), 0);
        let out = e.embed_batch(&["x"]).await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].is_empty());
    }
}
