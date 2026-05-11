//! `tower::Service<Query, Response = Vec<Hit>>` adapter.
//!
//! The retriever IS a tower service per blueprint §8 — composing with
//! `ServiceBuilder::new().rate_limit(..).timeout(..).retry(..).service(retriever)`
//! gives the entire ecosystem of tower middleware for free.
//!
//! ## Construction
//!
//! ```ignore
//! let svc = RetrievalService::new(root, embedder, storage, keyword);
//! let mut wrapped = ServiceBuilder::new()
//!     .rate_limit(10, Duration::from_secs(1))
//!     .timeout(Duration::from_secs(5))
//!     .service(svc);
//! let hits = wrapped.ready().await?.call(Query::text("foo")).await?;
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use lunaris_core::{Embedder, KeywordPort, LunarisError, Scope, StoragePort};
use tower::Service;

use crate::hydrate::hydrate;
use crate::operators::{QueryContext, Retriever};
use crate::types::{Hit, Query};

/// Boxed future for the tower::Service impl. We use a `BoxFuture` rather than
/// a named future type because Service futures must be `'static + Send` and
/// the inner retriever's future is opaque.
type BoxFuture = Pin<Box<dyn Future<Output = Result<Vec<Hit>, LunarisError>> + Send + 'static>>;

/// `tower::Service` adapter wrapping a built operator tree.
///
/// Cloneable — every clone shares the same `Arc<dyn Retriever>` so wrapping
/// in a `Buffer` / `Limit` layer is cheap.
#[derive(Clone)]
pub struct RetrievalService {
    pub(crate) root: Arc<dyn Retriever>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) storage: Arc<dyn StoragePort>,
    pub(crate) keyword: Arc<dyn KeywordPort>,
}

impl RetrievalService {
    pub fn new(
        root: Arc<dyn Retriever>,
        embedder: Arc<dyn Embedder>,
        storage: Arc<dyn StoragePort>,
        keyword: Arc<dyn KeywordPort>,
    ) -> Self {
        Self { root, embedder, storage, keyword }
    }
}

impl Service<Query> for RetrievalService {
    type Response = Vec<Hit>;
    type Error = LunarisError;
    type Future = BoxFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Always ready — backpressure happens inside the operator tree (vector
        // search timeout / keyword search timeout / etc.). Tower's middleware
        // (rate_limit / concurrency_limit) wraps THIS service for shared
        // backpressure across callers.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, q: Query) -> Self::Future {
        let storage = self.storage.clone();
        let embedder = self.embedder.clone();
        let keyword = self.keyword.clone();
        let root = self.root.clone();
        let as_of = q.as_of;

        Box::pin(async move {
            // Wave 2.5A/2.5C: RetrievalService has no scope context — use dev
            // scope as the default. Callers needing scope isolation should use
            // RetrievalBuilder (via Lunaris::recall() / ScopedLunaris::dsl()).
            let ctx = QueryContext::new(q, Scope::dev(), embedder, storage.clone(), keyword);
            let raw = root.retrieve(&ctx).await?;
            // Plan 04-04 B-9: RetrievalService callers don't have a verifier
            // queue-depth check (only `Lunaris::recall_with_degraded_check`
            // does), so initial_degraded is unconditionally false here.
            hydrate(storage.as_ref(), raw, as_of, false).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;

    #[test]
    fn retrieval_service_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RetrievalService>();
    }

    #[test]
    fn future_type_is_static_send() {
        // Sanity: future type is `'static + Send` per Service contract.
        // TypeId comparison is a compile-time-ish proxy.
        let _ = TypeId::of::<BoxFuture>();
    }
}
