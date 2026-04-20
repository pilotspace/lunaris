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

fn det_vec(s: &str, dim: usize) -> Vec<f32> {
    // Deterministic: seed a tiny LCG by hashing the input; emit `dim` floats in [-1, 1].
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    let mut state = h.finish().max(1);
    (0..dim)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = ((state >> 33) as u32) as f32 / u32::MAX as f32;
            v * 2.0 - 1.0
        })
        .collect()
}
