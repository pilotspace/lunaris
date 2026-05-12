//! Retriever trait + per-call query context shared across the operator tree.

pub mod combinators;
pub mod degraded;
pub mod fuse;
pub mod graph;
pub mod keyword;
pub mod modifiers;
pub mod recency;
pub mod rerank;
pub mod vector;

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::{Embedder, KeywordPort, LunarisError, Scope, StoragePort};
use tokio::sync::OnceCell;

use crate::types::{Query, RawHit};

/// Object-safe async retriever.
///
/// Operators implement this trait. Combinators (`AndRetriever`, `OrRetriever`,
/// `ThenRetriever`) take inner `Box<dyn Retriever>`s and dispatch to them
/// concurrently or sequentially. The outermost layer is the
/// [`crate::service::RetrievalService`] which adapts the trait into the
/// canonical `tower::Service<Query>` shape.
///
/// ## `as_any` for tree introspection
///
/// `as_any` is a non-default method that every concrete operator overrides
/// with `fn as_any(&self) -> &dyn Any { self }`. The Phase 1.5 RRF routing
/// dispatcher (`fusion::inspect_branches`) downcasts via this method to peek
/// at the operator tree shape. The default impl returns `&()` — a no-op
/// `&dyn Any` that downcasts fail on so any third-party operator that
/// forgets to override the method gracefully degrades to client-side fusion.
#[async_trait]
pub trait Retriever: Send + Sync {
    /// Run the operator against `ctx` and return the unranked-or-ranked
    /// raw hits this operator produces. The downstream stage (combinator
    /// or `fuse_rrf`) decides what to do with them.
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError>;

    /// Cast this operator to `&dyn Any` so the RRF routing dispatcher can
    /// recover the concrete operator type. Concrete operators override with
    /// `fn as_any(&self) -> &dyn Any { self }`. The default returns a sentinel
    /// `&()` so unknown operators downcast as `None` (and the dispatcher falls
    /// back to client-side RRF instead of misrouting).
    fn as_any(&self) -> &dyn Any {
        // Sentinel: `&()` is `'static + Any` and downcasts to nothing useful.
        &()
    }
}

/// Shared per-call state passed down the operator tree.
///
/// Cached query embedding lives here — `Vector` calls [`QueryContext::embed_once`]
/// which goes through a `tokio::sync::OnceCell` so concurrent `.and(Vector, X)`
/// branches share the embedding instead of re-embedding the query.
///
/// `moon_storage` is an optional concrete-typed handle to the underlying
/// `MoonStorage` backend, populated when the builder knows the storage Arc
/// points to a Moon backend. The Phase 1.5 `fuse_rrf` Moon-native dispatch
/// reads this to invoke `text().hybrid_search()` in one round trip — without
/// it the dispatch falls back to client-side RRF even when the backend
/// reports `capabilities().native_rrf == true` (graceful degradation).
///
/// ## RFC 0001 Wave 2.5A + 2.5C: `scope` field
///
/// `scope` carries the per-call partition scope so every operator dispatches to
/// the correct scope-isolated index without needing `Scope::dev()` placeholders.
/// Wave 2.5A adds the field (defaulting to `Scope::dev()` for bare `Lunaris::recall()`
/// callers); Wave 2.5C seeds it from `RetrievalBuilder::with_scope` so
/// `ScopedLunaris::recall()` routes to the correct scope at the storage layer.
pub struct QueryContext {
    pub query: Query,
    pub scope: Scope,
    pub embedder: Arc<dyn Embedder>,
    pub storage: Arc<dyn StoragePort>,
    pub keyword: Arc<dyn KeywordPort>,
    pub query_embedding: OnceCell<Vec<f32>>,
    pub moon_storage: Option<Arc<lunaris_storage_moon::MoonStorage>>,
}

impl QueryContext {
    /// Construct a fresh context with an explicit scope.
    /// Builders typically create one of these per `recall().execute()` call.
    pub fn new(
        query: Query,
        scope: Scope,
        embedder: Arc<dyn Embedder>,
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
    ) -> Self {
        Self {
            query,
            scope,
            embedder,
            storage,
            keyword,
            query_embedding: OnceCell::new(),
            moon_storage: None,
        }
    }

    /// Construct a context with the Moon-native handle pre-wired so the
    /// Phase 1.5 fuse_rrf Moon-native path is available. The dispatcher
    /// checks `ctx.moon_storage.is_some()` AND
    /// `ctx.storage.capabilities().native_rrf` — both must hold.
    pub fn with_moon(
        query: Query,
        scope: Scope,
        embedder: Arc<dyn Embedder>,
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
        moon: Arc<lunaris_storage_moon::MoonStorage>,
    ) -> Self {
        Self {
            query,
            scope,
            embedder,
            storage,
            keyword,
            query_embedding: OnceCell::new(),
            moon_storage: Some(moon),
        }
    }

    /// Embed the query exactly once for the lifetime of this context.
    ///
    /// Concurrent callers race to populate the `OnceCell`; `tokio::sync::OnceCell`
    /// guarantees the closure runs at most once and other callers await the
    /// in-flight initializer.
    pub async fn embed_once(&self) -> Result<Vec<f32>, LunarisError> {
        self.query_embedding
            .get_or_try_init(|| async {
                let v = self.embedder.embed_batch(&[self.query.text.as_str()]).await?;
                Ok::<Vec<f32>, LunarisError>(
                    v.into_iter().next().expect("non-empty batch from one input"),
                )
            })
            .await
            .cloned()
    }
}

/// Helper: clamp a user-supplied `k` to `MAX_K` to defend against DoS via
/// `k = 1_000_000` (T-02-02-03 mitigation). Emits a `tracing::warn!` so
/// observability catches the clamp.
pub const MAX_K: usize = 1000;

#[inline]
pub fn clamp_k(k: usize) -> usize {
    if k > MAX_K {
        tracing::warn!(requested = k, max = MAX_K, "k clamped to MAX_K");
        MAX_K
    } else {
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_k_clamps_at_max() {
        assert_eq!(clamp_k(1_000_000), MAX_K);
        assert_eq!(clamp_k(MAX_K), MAX_K);
        assert_eq!(clamp_k(0), 0);
        assert_eq!(clamp_k(30), 30);
    }
}
