//! RFC 0007 §3 — `FallbackEmbedder<P, F>` static-dispatch combinator.
//!
//! Mirror of [`FallbackExtractor`](lunaris_extract::fallback::FallbackExtractor)
//! over the [`Embedder`] trait. Static dispatch keeps the per-chunk embed
//! hot path monomorphised (RFC §2.2 #4 — `Box<dyn Embedder>` on the inner
//! loop costs measurable p99).
//!
//! ## Dimension contract
//!
//! [`Embedder::dim`] MUST agree between primary and fallback. The
//! constructor accepts both and panics in debug builds when they
//! disagree; in release builds the mismatch surfaces as a downstream
//! "vector dim mismatch" `StorageError`. We could enforce statically
//! with a const-generic `Embedder<const DIM: usize>` but that's a
//! breaking trait change — deferred until v0.3.
//!
//! ## Transient vs terminal
//!
//! Same classifier as [`lunaris_extract::fallback::is_transient`] —
//! transports + 5xx are transient; schema-level errors are terminal.
//! `Embedder` errors flow through `LunarisError` so the policy
//! is reusable.

use std::sync::Arc;

use async_trait::async_trait;

use lunaris_core::circuit_breaker::CircuitBreaker;
use lunaris_core::{Embedder, LunarisError, StorageError};

/// Opaque provider tag — tracing label only. Mirrors
/// [`lunaris_extract::fallback::ProviderId`] but lives here so
/// downstream callers don't need a lunaris-extract dep for the embedder
/// stack.
#[derive(Clone, Debug)]
pub struct EmbedderProviderId(String);

impl EmbedderProviderId {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Two-arm static-dispatch fallback embedder. Stacks recursively.
pub struct FallbackEmbedder<P, F>
where
    P: Embedder,
    F: Embedder,
{
    primary: P,
    fallback: F,
    breaker: Arc<CircuitBreaker>,
    provider_id: EmbedderProviderId,
}

impl<P, F> FallbackEmbedder<P, F>
where
    P: Embedder,
    F: Embedder,
{
    /// Construct a fallback chain. Debug-only assertion: `primary.dim()
    /// == fallback.dim()` so the downstream vector-store doesn't see
    /// dimension-mismatched rows. In release builds a mismatch surfaces
    /// at the storage boundary as a `StorageError::Backend(...)` with a
    /// dim-mismatch payload.
    pub fn new(primary: P, fallback: F, provider_id: EmbedderProviderId) -> Self {
        debug_assert_eq!(
            primary.dim(),
            fallback.dim(),
            "FallbackEmbedder: primary.dim() {} != fallback.dim() {}",
            primary.dim(),
            fallback.dim()
        );
        Self {
            primary,
            fallback,
            breaker: Arc::new(CircuitBreaker::new()),
            provider_id,
        }
    }

    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    #[must_use]
    pub fn breaker(&self) -> &Arc<CircuitBreaker> {
        &self.breaker
    }

    #[must_use]
    pub fn provider_id(&self) -> &EmbedderProviderId {
        &self.provider_id
    }
}

#[async_trait]
impl<P, F> Embedder for FallbackEmbedder<P, F>
where
    P: Embedder,
    F: Embedder,
{
    fn dim(&self) -> usize {
        // Primary's dim is the source of truth; we asserted parity in `new`.
        self.primary.dim()
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if self.breaker.allow_request() {
            match self.primary.embed_batch(inputs).await {
                Ok(out) => {
                    self.breaker.on_success();
                    tracing::trace!(
                        provider = %self.provider_id.as_str(),
                        "fallback_embedder.primary.success"
                    );
                    return Ok(out);
                }
                Err(e) if is_transient(&e) => {
                    self.breaker.on_failure();
                    tracing::warn!(
                        provider = %self.provider_id.as_str(),
                        error = %e,
                        "fallback_embedder.primary.transient_failure — routing to fallback"
                    );
                    // fall through
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %self.provider_id.as_str(),
                        error = %e,
                        "fallback_embedder.primary.terminal_failure — not retrying"
                    );
                    return Err(e);
                }
            }
        } else {
            tracing::debug!(
                provider = %self.provider_id.as_str(),
                "fallback_embedder.primary.tripped — breaker open"
            );
        }

        self.fallback.embed_batch(inputs).await
    }
}

/// Mirror of `lunaris_extract::fallback::is_transient` — exposed here so
/// the embedder side doesn't need a lunaris-extract dep just for the
/// classifier function.
#[must_use]
pub fn is_transient(err: &LunarisError) -> bool {
    match err {
        LunarisError::Storage(StorageError::Backend(_)) => true,
        LunarisError::Storage(StorageError::NotSupported(_)) => false,
        LunarisError::Storage(_) => false,
        LunarisError::Extract(lunaris_core::ExtractError::Timeout) => true,
        LunarisError::Extract(lunaris_core::ExtractError::Backend(_)) => true,
        LunarisError::Extract(lunaris_core::ExtractError::GrammarReject(_)) => false,
        LunarisError::Validate(_) => false,
        LunarisError::Retrieve(_) => false,
        LunarisError::Consolidate(_) => false,
        // LunarisError + StorageError + ExtractError are #[non_exhaustive].
        // Be conservative on novel variants: treat as terminal so we never
        // silently mask a future error class via fallback.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Scripted embedder — pops pre-canned results from a queue.
    struct ScriptedEmbedder {
        dim: usize,
        results: Mutex<std::collections::VecDeque<Result<Vec<Vec<f32>>, LunarisError>>>,
        calls: AtomicUsize,
    }
    impl ScriptedEmbedder {
        fn new(dim: usize, results: Vec<Result<Vec<Vec<f32>>, LunarisError>>) -> Self {
            Self {
                dim,
                results: Mutex::new(results.into()),
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }
    #[async_trait]
    impl Embedder for ScriptedEmbedder {
        fn dim(&self) -> usize {
            self.dim
        }
        async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(vec![vec![0.0_f32; self.dim]; inputs.len()]))
        }
    }

    fn ok_vec(dim: usize) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(vec![vec![0.1; dim]])
    }
    fn transient_err() -> Result<Vec<Vec<f32>>, LunarisError> {
        Err(LunarisError::Storage(StorageError::Backend("upstream 503".into())))
    }
    fn terminal_err() -> Result<Vec<Vec<f32>>, LunarisError> {
        Err(LunarisError::Validate(lunaris_core::ValidateError::Temporal))
    }

    #[tokio::test]
    async fn primary_success_skips_fallback() {
        let primary = ScriptedEmbedder::new(768, vec![ok_vec(768)]);
        let fallback = ScriptedEmbedder::new(768, vec![ok_vec(768)]);
        let f =
            FallbackEmbedder::new(primary, fallback, EmbedderProviderId::new("test"));
        let _ = f.embed_batch(&["hello"]).await.unwrap();
        assert_eq!(f.primary.calls(), 1);
        assert_eq!(f.fallback.calls(), 0);
    }

    #[tokio::test]
    async fn transient_failure_routes_to_fallback() {
        let primary = ScriptedEmbedder::new(768, vec![transient_err()]);
        let fallback = ScriptedEmbedder::new(768, vec![ok_vec(768)]);
        let f =
            FallbackEmbedder::new(primary, fallback, EmbedderProviderId::new("test"));
        let _ = f.embed_batch(&["hello"]).await.unwrap();
        assert_eq!(f.primary.calls(), 1);
        assert_eq!(f.fallback.calls(), 1);
    }

    #[tokio::test]
    async fn terminal_failure_propagates() {
        let primary = ScriptedEmbedder::new(768, vec![terminal_err()]);
        let fallback = ScriptedEmbedder::new(768, vec![ok_vec(768)]);
        let f =
            FallbackEmbedder::new(primary, fallback, EmbedderProviderId::new("test"));
        let err = f.embed_batch(&["hello"]).await.unwrap_err();
        assert!(matches!(err, LunarisError::Validate(_)));
        assert_eq!(f.primary.calls(), 1);
        assert_eq!(f.fallback.calls(), 0, "terminal failure must not mask via fallback");
    }

    #[tokio::test]
    async fn breaker_trips_after_threshold() {
        let primary_calls: Vec<_> = (0..6)
            .map(|i| if i < 5 { transient_err() } else { ok_vec(768) })
            .collect();
        let fallback_calls: Vec<_> = (0..6).map(|_| ok_vec(768)).collect();
        let primary = ScriptedEmbedder::new(768, primary_calls);
        let fallback = ScriptedEmbedder::new(768, fallback_calls);
        let f =
            FallbackEmbedder::new(primary, fallback, EmbedderProviderId::new("test"));

        for _ in 0..5 {
            let _ = f.embed_batch(&["x"]).await.unwrap();
        }
        assert_eq!(f.primary.calls(), 5);
        assert_eq!(f.fallback.calls(), 5);

        let _ = f.embed_batch(&["x"]).await.unwrap();
        assert_eq!(f.primary.calls(), 5, "breaker Open: primary skipped");
        assert_eq!(f.fallback.calls(), 6);
    }

    #[test]
    fn dim_returns_primary() {
        let primary = ScriptedEmbedder::new(768, vec![]);
        let fallback = ScriptedEmbedder::new(768, vec![]);
        let f =
            FallbackEmbedder::new(primary, fallback, EmbedderProviderId::new("test"));
        assert_eq!(f.dim(), 768);
    }

    #[test]
    #[should_panic(expected = "FallbackEmbedder: primary.dim()")]
    fn debug_assert_mismatched_dim_panics() {
        // debug-build only — release builds skip the assert and the
        // mismatch surfaces downstream.
        let primary = ScriptedEmbedder::new(768, vec![]);
        let fallback = ScriptedEmbedder::new(384, vec![]);
        let _ = FallbackEmbedder::new(primary, fallback, EmbedderProviderId::new("test"));
    }
}
