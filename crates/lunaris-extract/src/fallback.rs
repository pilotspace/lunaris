//! RFC 0007 §3 — `FallbackExtractor<P, F>` static-dispatch combinator.
//!
//! Wraps a **primary** extractor and a **fallback** extractor. The primary
//! drives a per-instance [`CircuitBreaker`] (from `lunaris-core`):
//!
//! - When the breaker is Closed / HalfOpen, the primary runs first.
//!   Success → return primary's result. Transient failure → record failure
//!   on the breaker and try fallback. Terminal failure → return the error
//!   verbatim (don't mask).
//! - When the breaker is Open, skip the primary entirely and drive
//!   straight to the fallback.
//!
//! Stacks recursively. `FallbackExtractor::new(a, FallbackExtractor::new(b, c))`
//! gives a 3-deep stack with one breaker per layer (each layer's primary
//! has its own breaker). Cross-layer state sharing is an RFC §7 open
//! question — pass an explicit `Arc<CircuitBreaker>` to
//! [`FallbackExtractor::with_breaker`] for shared state.
//!
//! ## Transient vs terminal (RFC 0007 §3.3)
//!
//! Only transient errors trip the breaker AND fall through to the
//! fallback. Terminal errors are masked-safe to propagate — if your
//! caller's data is malformed (`GrammarReject`), retrying with a
//! different provider won't help. The current heuristic:
//!
//! - **Transient**: `Storage(Backend(...))` (network / 5xx upstream),
//!   `Extract(Timeout)`, `Extract(Backend(...))` (transport-level).
//! - **Terminal**: `Extract(GrammarReject)` (schema-level — your
//!   prompt is wrong), `Validate(...)` (data-level), `Storage(NotSupported)`.
//!
//! The classifier lives in [`is_transient`] so callers + tests can
//! introspect the policy.

use std::sync::Arc;

use async_trait::async_trait;
use ulid::Ulid;

use lunaris_core::circuit_breaker::CircuitBreaker;
use lunaris_core::{ExtractError, LunarisError, StorageError};

use crate::types::ChunkInput;
use crate::{Extractor, RawExtractionBatch};

/// Opaque provider tag — used for breaker bookkeeping + tracing labels.
/// Keep stable across releases; downstream metrics dashboards depend on
/// these strings.
#[derive(Clone, Debug)]
pub struct ProviderId(String);

impl ProviderId {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Two-arm static-dispatch fallback extractor.
///
/// `P` and `F` are concrete extractor types — *not* `dyn Extractor`.
/// This keeps the ingest hot path monomorphised (RFC 0007 §2.2 #4).
pub struct FallbackExtractor<P, F>
where
    P: Extractor,
    F: Extractor,
{
    primary: P,
    fallback: F,
    breaker: Arc<CircuitBreaker>,
    provider_id: ProviderId,
}

impl<P, F> FallbackExtractor<P, F>
where
    P: Extractor,
    F: Extractor,
{
    /// Construct a fallback chain with a fresh per-instance breaker.
    pub fn new(primary: P, fallback: F, provider_id: ProviderId) -> Self {
        Self { primary, fallback, breaker: Arc::new(CircuitBreaker::new()), provider_id }
    }

    /// Construct with an explicit `Arc<CircuitBreaker>` so two
    /// `FallbackExtractor` instances pointing at the same upstream can
    /// share breaker state. RFC §7 open-question accommodation.
    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    /// Borrow the breaker — primarily for metrics / observability /
    /// tests asserting the state machine.
    #[must_use]
    pub fn breaker(&self) -> &Arc<CircuitBreaker> {
        &self.breaker
    }

    #[must_use]
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
}

#[async_trait]
impl<P, F> Extractor for FallbackExtractor<P, F>
where
    P: Extractor,
    F: Extractor,
{
    async fn extract(
        &self,
        episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if self.breaker.allow_request() {
            match self.primary.extract(episode_id, chunks).await {
                Ok(batch) => {
                    self.breaker.on_success();
                    tracing::trace!(
                        provider = %self.provider_id.as_str(),
                        "fallback.primary.success"
                    );
                    return Ok(batch);
                }
                Err(e) if is_transient(&e) => {
                    self.breaker.on_failure();
                    tracing::warn!(
                        provider = %self.provider_id.as_str(),
                        error = %e,
                        "fallback.primary.transient_failure — routing to fallback"
                    );
                    // fall through to fallback
                }
                Err(e) => {
                    // Terminal — propagate without masking.
                    tracing::warn!(
                        provider = %self.provider_id.as_str(),
                        error = %e,
                        "fallback.primary.terminal_failure — not retrying"
                    );
                    return Err(e);
                }
            }
        } else {
            tracing::debug!(
                provider = %self.provider_id.as_str(),
                "fallback.primary.tripped — breaker open"
            );
        }

        self.fallback.extract(episode_id, chunks).await
    }

    fn applies(&self) -> bool {
        self.primary.applies() || self.fallback.applies()
    }
}

/// Classify a `LunarisError` as transient (retry-worthy via fallback)
/// or terminal (do not mask). Exposed so tests + downstream callers can
/// introspect the policy.
#[must_use]
pub fn is_transient(err: &LunarisError) -> bool {
    match err {
        LunarisError::Storage(StorageError::Backend(_)) => true,
        LunarisError::Storage(StorageError::NotSupported(_)) => false,
        LunarisError::Storage(_) => false,
        LunarisError::Extract(ExtractError::Timeout) => true,
        LunarisError::Extract(ExtractError::Backend(_)) => true,
        LunarisError::Extract(ExtractError::GrammarReject(_)) => false,
        LunarisError::Validate(_) => false,
        LunarisError::Retrieve(_) => false,
        LunarisError::Consolidate(_) => false,
        // LunarisError is #[non_exhaustive] — be conservative on unknown
        // future variants: treat as terminal so we never silently mask a
        // novel error class via fallback.
        _ => false,
    }
}

/// Wrap a real extractor with the production fallback floor: a
/// [`FallbackExtractor`] whose fallback is `NoopExtractor`. This is the seam
/// `Lunaris::open`'s `default_extractor` calls so the cache-hit extractor is
/// breaker-guarded — a transient primary failure degrades to empty extraction
/// (graph extraction off for that episode) instead of failing ingest, while a
/// terminal error propagates unchanged.
///
/// `provider` is the breaker / tracing label (e.g. `"gemma-3-4b-it"`).
#[must_use]
pub fn fallback_wrap<P: Extractor>(primary: P, provider: &str) -> Arc<dyn Extractor> {
    Arc::new(FallbackExtractor::new(primary, crate::NoopExtractor, ProviderId::new(provider)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Scripted extractor — pop pre-canned results from the front.
    /// Tracks call count so tests can assert primary-vs-fallback dispatch.
    struct ScriptedExtractor {
        results: Mutex<std::collections::VecDeque<Result<RawExtractionBatch, LunarisError>>>,
        calls: AtomicUsize,
        applies: bool,
    }
    impl ScriptedExtractor {
        fn new(results: Vec<Result<RawExtractionBatch, LunarisError>>) -> Self {
            Self { results: Mutex::new(results.into()), calls: AtomicUsize::new(0), applies: true }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }
    #[async_trait]
    impl Extractor for ScriptedExtractor {
        async fn extract(
            &self,
            _episode_id: Ulid,
            _chunks: &[ChunkInput],
        ) -> Result<RawExtractionBatch, LunarisError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(RawExtractionBatch::default()))
        }
        fn applies(&self) -> bool {
            self.applies
        }
    }

    fn ok_batch() -> Result<RawExtractionBatch, LunarisError> {
        Ok(RawExtractionBatch::default())
    }
    fn transient_err() -> Result<RawExtractionBatch, LunarisError> {
        Err(LunarisError::Storage(StorageError::Backend("upstream 503".into())))
    }
    fn terminal_err() -> Result<RawExtractionBatch, LunarisError> {
        Err(LunarisError::Extract(ExtractError::GrammarReject("malformed".into())))
    }

    #[tokio::test]
    async fn primary_success_skips_fallback() {
        let primary = ScriptedExtractor::new(vec![ok_batch()]);
        let fallback = ScriptedExtractor::new(vec![ok_batch()]);
        let f = FallbackExtractor::new(primary, fallback, ProviderId::new("test"));
        let _ = f.extract(Ulid::new(), &[]).await.unwrap();
        assert_eq!(f.primary.calls(), 1);
        assert_eq!(f.fallback.calls(), 0, "fallback must not run on primary success");
    }

    #[tokio::test]
    async fn transient_failure_falls_through_to_fallback() {
        let primary = ScriptedExtractor::new(vec![transient_err()]);
        let fallback = ScriptedExtractor::new(vec![ok_batch()]);
        let f = FallbackExtractor::new(primary, fallback, ProviderId::new("test"));
        let _ = f.extract(Ulid::new(), &[]).await.unwrap();
        assert_eq!(f.primary.calls(), 1);
        assert_eq!(f.fallback.calls(), 1, "fallback must run on transient primary failure");
    }

    #[tokio::test]
    async fn terminal_failure_propagates_without_fallback() {
        let primary = ScriptedExtractor::new(vec![terminal_err()]);
        let fallback = ScriptedExtractor::new(vec![ok_batch()]);
        let f = FallbackExtractor::new(primary, fallback, ProviderId::new("test"));
        let err = f.extract(Ulid::new(), &[]).await.unwrap_err();
        assert!(matches!(err, LunarisError::Extract(ExtractError::GrammarReject(_))));
        assert_eq!(f.primary.calls(), 1);
        assert_eq!(f.fallback.calls(), 0, "terminal failure must NOT mask via fallback");
    }

    #[tokio::test]
    async fn breaker_trips_after_threshold() {
        // 5 transient failures trip the default breaker. Subsequent calls
        // skip primary entirely.
        let primary_calls = vec![
            transient_err(),
            transient_err(),
            transient_err(),
            transient_err(),
            transient_err(),
            // would be the 6th — shouldn't be invoked because breaker is Open
            ok_batch(),
        ];
        let fallback_calls: Vec<_> = (0..6).map(|_| ok_batch()).collect();
        let primary = ScriptedExtractor::new(primary_calls);
        let fallback = ScriptedExtractor::new(fallback_calls);
        let f = FallbackExtractor::new(primary, fallback, ProviderId::new("test"));

        for _ in 0..5 {
            let _ = f.extract(Ulid::new(), &[]).await.unwrap();
        }
        assert_eq!(f.primary.calls(), 5);
        assert_eq!(f.fallback.calls(), 5);

        // 6th call — primary is tripped, fallback runs directly.
        let _ = f.extract(Ulid::new(), &[]).await.unwrap();
        assert_eq!(f.primary.calls(), 5, "primary skipped while breaker Open");
        assert_eq!(f.fallback.calls(), 6);
    }

    #[test]
    fn is_transient_classifier() {
        assert!(is_transient(&LunarisError::Storage(StorageError::Backend("x".into()))));
        assert!(is_transient(&LunarisError::Extract(ExtractError::Timeout)));
        assert!(is_transient(&LunarisError::Extract(ExtractError::Backend("x".into()))));
        assert!(!is_transient(&LunarisError::Extract(ExtractError::GrammarReject("x".into()))));
        assert!(!is_transient(&LunarisError::Storage(StorageError::NotSupported("x"))));
    }

    #[tokio::test]
    async fn applies_is_true_if_either_arm_applies() {
        let mut primary = ScriptedExtractor::new(vec![]);
        primary.applies = false;
        let fallback = ScriptedExtractor::new(vec![]);
        let f = FallbackExtractor::new(primary, fallback, ProviderId::new("test"));
        assert!(f.applies());

        let mut primary2 = ScriptedExtractor::new(vec![]);
        primary2.applies = false;
        let mut fallback2 = ScriptedExtractor::new(vec![]);
        fallback2.applies = false;
        let f2 = FallbackExtractor::new(primary2, fallback2, ProviderId::new("test"));
        assert!(!f2.applies());
    }

    // ── extractor-fallback-wiring (Half A): the `fallback_wrap` production seam ──
    // `fallback_wrap(primary, provider)` is the exact code `default_extractor`'s
    // candle cache-hit arm calls; it pins NoopExtractor as the fallback floor.
    // The return is `Arc<dyn Extractor>` (type-erased), so these assert on
    // observable behavior, not internal call counts.

    #[tokio::test]
    async fn fallback_wrap_transient_degrades_to_noop() {
        let wrapped = fallback_wrap(ScriptedExtractor::new(vec![transient_err()]), "gemma-3-4b-it");
        let chunks = vec![
            ChunkInput {
                chunk_id: Ulid::new(),
                text: "a".into(),
                heading_path: vec![],
                reference_time_iso: None,
            },
            ChunkInput {
                chunk_id: Ulid::new(),
                text: "b".into(),
                heading_path: vec![],
                reference_time_iso: None,
            },
        ];
        let batch = wrapped
            .extract(Ulid::new(), &chunks)
            .await
            .expect("transient primary must degrade to the NoopExtractor floor, not error");
        assert_eq!(batch.by_chunk.len(), 2, "NoopExtractor emits one empty extraction per chunk");
        assert!(
            batch
                .by_chunk
                .iter()
                .all(|r| r.entities.is_empty() && r.relations.is_empty() && r.facts.is_empty()),
            "the fallback floor produces empty extractions"
        );
    }

    #[tokio::test]
    async fn fallback_wrap_terminal_propagates() {
        let wrapped = fallback_wrap(ScriptedExtractor::new(vec![terminal_err()]), "gemma-3-4b-it");
        let err = wrapped
            .extract(Ulid::new(), &[])
            .await
            .expect_err("terminal primary error must propagate, not fall back to Noop");
        assert!(
            matches!(err, LunarisError::Extract(ExtractError::GrammarReject(_))),
            "terminal error propagates unchanged, got {err:?}"
        );
    }
}
