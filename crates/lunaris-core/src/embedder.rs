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

    /// Lower-priority batch embed for background/bulk work (e.g. ingest
    /// promotion). Semantically identical to [`Embedder::embed_batch`] — same
    /// vectors, same order — but signals the embedder may defer this batch
    /// behind interactive recall queries. The default just delegates; only a
    /// backend with an internal scheduler (the llama.cpp worker) overrides it
    /// to ride a background lane so ingest never head-of-line-blocks recall.
    async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        self.embed_batch(inputs).await
    }
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
/// Moon's `FT.CREATE` requires `dim > 0`, so `NoopEmbedder::new(0)` surfaces
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

/// Number of clusters [`det_vec`] draws from. At the 1M-vector corpus the
/// benchmarks build this averages ~4k vectors per cluster — dense enough that
/// a nearest-neighbour query has an unambiguous answer.
const DET_VEC_CLUSTERS: u64 = 256;

/// How far a vector strays from its cluster centroid, before normalisation.
const DET_VEC_JITTER: f32 = 0.15;

#[inline]
fn lcg(state: &mut u64) -> f32 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    // `>> 32`, not `>> 33`. The original shifted 33 bits, leaving 31 bits of
    // entropy in a value divided by `u32::MAX`, so the quotient never exceeded
    // ~0.5 and `q * 2.0 - 1.0` never became positive. Every "uniform [-1, 1]"
    // vector this crate ever produced sat in the all-negative orthant, which
    // put every pair at high cosine similarity — the opposite of the spread
    // the comment promised.
    ((*state >> 32) as u32) as f32 / u32::MAX as f32 * 2.0 - 1.0
}

/// Deterministic stand-in for a real embedding: unit-norm and clustered.
///
/// Two things were wrong with the previous generator. It claimed to emit
/// "floats in [-1, 1]" and emitted [-1, 0] (see `lcg`), so every vector sat
/// in the all-negative orthant at high mutual cosine similarity. And the
/// coordinates were independent, which in high dimension puts every pair at
/// nearly the same distance — "the nearest neighbour" becomes a coin-flip
/// among thousands of ties. A real embedder does neither: its output is
/// L2-normalised and concentrated near a low-dimensional manifold, and that
/// structure is the whole reason an ANN index can index it. A test double
/// without it makes every similarity-search test unrepresentative of the
/// system it stands in for.
///
/// Scope note, so nobody re-derives a story this does not support: this was
/// written while chasing a Moon compaction wedge (`merge recall 0.0000` →
/// `MOONERR: busy: compaction backlog`), on the theory that tie-saturated
/// vectors were collapsing Moon's merge-recall check. **That theory was
/// tested and refuted** — the wedge reproduces identically with these
/// clustered vectors. The fix stands on its own merits; it is not a fix for
/// the wedge.
///
/// Determinism is unchanged: the same string always yields the same vector.
pub fn det_vec(s: &str, dim: usize) -> Vec<f32> {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    let hashed = h.finish().max(1);

    // The centroid depends only on the cluster, so every string landing in a
    // cluster shares it; the jitter depends on the string, so members stay
    // distinct.
    let mut centroid_state = (hashed % DET_VEC_CLUSTERS).wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
    let mut jitter_state = hashed;

    let mut v: Vec<f32> = (0..dim)
        .map(|_| lcg(&mut centroid_state) + lcg(&mut jitter_state) * DET_VEC_JITTER)
        .collect();

    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod det_vec_tests {
    use super::*;

    /// The doc on `det_vec` claims the vectors are clustered on the unit
    /// sphere. Assert the geometry, not the prose: uniform noise passes a
    /// "looks like floats" check just as well.
    #[test]
    fn det_vec_is_unit_norm_and_actually_clustered() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let dim = 768;
        let cluster_of = |s: &str| {
            let mut h = DefaultHasher::new();
            s.hash(&mut h);
            h.finish().max(1) % DET_VEC_CLUSTERS
        };
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();

        let labels: Vec<String> = (0..4000).map(|i| format!("fact-{i}")).collect();
        for l in labels.iter().take(50) {
            let v = det_vec(l, dim);
            assert!((dot(&v, &v) - 1.0).abs() < 1e-3, "{l}: not unit-norm ({})", dot(&v, &v));
        }

        // Average cosine within a cluster vs across clusters. On uniform noise
        // both land near 0 and the gap vanishes; that indistinguishability is
        // exactly what collapsed Moon's merge-recall check.
        let (mut same, mut same_n, mut diff, mut diff_n) = (0.0f32, 0u32, 0.0f32, 0u32);
        for (i, a) in labels.iter().enumerate().take(300) {
            let va = det_vec(a, dim);
            for b in labels.iter().skip(i + 1).take(300) {
                let cos = dot(&va, &det_vec(b, dim));
                if cluster_of(a) == cluster_of(b) {
                    same += cos;
                    same_n += 1;
                } else {
                    diff += cos;
                    diff_n += 1;
                }
            }
        }
        assert!(same_n > 50 && diff_n > 50, "too few pairs: {same_n}/{diff_n}");
        let (same, diff) = (same / same_n as f32, diff / diff_n as f32);
        assert!(
            same - diff > 0.5,
            "cluster structure is too weak for an ANN index to find: \
             same-cluster cosine {same:.3} vs cross-cluster {diff:.3}"
        );
    }
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

#[cfg(test)]
mod lowpri_default_tests {
    use super::*;

    #[tokio::test]
    async fn lowpri_defaults_to_embed_batch() {
        // scenario: non-llamacpp embedders unaffected — an Embedder that does
        // NOT override embed_batch_lowpri gets the default, byte-identical to
        // embed_batch in value AND input order.
        let e = StubEmbedder::new(768);
        let inputs = ["alpha", "beta", "gamma"];
        let hi = e.embed_batch(&inputs).await.unwrap();
        let lo = e.embed_batch_lowpri(&inputs).await.unwrap();
        assert_eq!(hi, lo, "default embed_batch_lowpri must byte-match embed_batch");
    }

    #[tokio::test]
    async fn lowpri_empty_is_noop() {
        // scenario: empty input is a no-op on both lanes.
        let e = StubEmbedder::new(768);
        assert!(e.embed_batch_lowpri(&[]).await.unwrap().is_empty());
    }
}
