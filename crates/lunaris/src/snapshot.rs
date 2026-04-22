//! Plan 08-00 — `Lunaris::snapshot()` monotonic LSN marker.
//!
//! Exposes a cheap consistent-snapshot primitive to bindings (PyO3, napi-rs
//! in Plans 08-02 / 08-03) and to callers doing time-travel reads.
//! Implemented as a no-op `StoragePort::atomic_write(&[])` — the cheapest way
//! to observe the current LSN from the standard trait, as that path is the
//! only public trait method that returns an [`Lsn`] with a guarantee of
//! monotonicity (v0.1.0 Phase 1 contract). A NEW call always observes an LSN
//! strictly greater than every LSN returned by any prior successful call on
//! the same backend.
//!
//! # Scope (revision iteration 2)
//!
//! Plan 08-00 lands **exactly one** addition to the umbrella crate:
//! `Lunaris::snapshot()`. Earlier revisions also added a `ZeroEmbedder` in
//! `lunaris-embed` behind a `mock-embedder` feature AND a `with_clock` test
//! seam in `lunaris`. Both were dropped after realigning Phase 8 success
//! criterion #4 against the ROADMAP verbatim — the "byte-identical" guarantee
//! compares **backends** (Moon == Postgres) per single driver run, NOT
//! languages (Rust == Py == TS). `lunaris-core/src/hlc.rs` +
//! `lunaris-core/src/primitives.rs` + `lunaris-embed` remain untouched.
//!
//! # Future optimization
//!
//! Expose a native `StoragePort::current_lsn()` read that avoids the WAL
//! append. Out of scope for v0.1.1 — the no-op atomic_write is still
//! microseconds on Moon and matches the "cheapest consistent" promise.

use lunaris_core::{Lsn, LunarisError};

use crate::handle::Lunaris;

impl Lunaris {
    /// Returns the current [`Lsn`] — a monotonic snapshot marker.
    ///
    /// Callers can use this as an `as_of` witness for time-travel reads, or
    /// to verify that subsequent operations observe a consistent view of
    /// state. Two sequential calls on the same handle satisfy
    /// `call_n.lsn < call_{n+1}.lsn` (strict monotonicity from
    /// [`lunaris_core::StoragePort::atomic_write`]).
    ///
    /// Implementation: issues a no-op `atomic_write(&[])`. The empty `ops`
    /// slice guarantees no data changes; the backend still advances its LSN
    /// per the atomic_write monotonicity rule. Errors from the storage layer
    /// flow through the `#[from]`-derived `LunarisError::Storage` variant.
    pub async fn snapshot(&self) -> Result<Lsn, LunarisError> {
        self.storage
            .atomic_write(&[])
            .await
            .map_err(LunarisError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use lunaris_core::{
        CypherQuery, Embedder, Filter, GraphResult, Hlc, HlcClock, QueueMsg, Row,
        StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit, WriteOp,
    };
    use parking_lot::Mutex;

    /// Minimal in-memory storage that increments an internal counter on each
    /// `atomic_write` and returns it as an [`Lsn`]. Every other method returns
    /// `NotSupported` — this is a snapshot-only test double modelling the
    /// v0.1.0 `StoragePort::atomic_write` monotonicity contract.
    #[derive(Default)]
    struct MonotonicLsnStorage {
        counter: Mutex<u32>,
    }

    #[async_trait]
    impl StoragePort for MonotonicLsnStorage {
        async fn atomic_write(&self, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
            let mut g = self.counter.lock();
            *g += 1;
            // `Lsn` is `(wall_ms, counter)` lex-ordered; holding wall_ms
            // constant and bumping counter exercises the strict-monotonic
            // path without wall-clock noise.
            Ok(Lsn { wall_ms: 1, counter: *g })
        }
        async fn vector_search(
            &self,
            _index: &str,
            _query: &[f32],
            _k: usize,
            _filter: Option<&Filter>,
            _as_of: Option<Hlc>,
            _rerank: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            Err(StorageError::NotSupported("MonotonicLsnStorage::vector_search"))
        }
        async fn graph_traverse(
            &self,
            _query: &CypherQuery,
            _as_of: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            Err(StorageError::NotSupported("MonotonicLsnStorage::graph_traverse"))
        }
        async fn scan_range(
            &self,
            _prefix: &[u8],
            _as_of: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            Err(StorageError::NotSupported("MonotonicLsnStorage::scan_range"))
        }
        async fn read_as_of(
            &self,
            _key: &[u8],
            _as_of: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            Err(StorageError::NotSupported("MonotonicLsnStorage::read_as_of"))
        }
        async fn publish(
            &self,
            _topic: &str,
            _partition: u16,
            _payload: Bytes,
        ) -> Result<u64, StorageError> {
            Err(StorageError::NotSupported("MonotonicLsnStorage::publish"))
        }
        async fn subscribe(
            &self,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            Err(StorageError::NotSupported("MonotonicLsnStorage::subscribe"))
        }
        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities {
                bi_temporal_native: false,
                graph_native: false,
                rerank_native: false,
                queue_native: false,
                max_vector_dim: 768,
                native_rrf: false,
            }
        }
    }

    /// Second test double — `atomic_write` always fails. Proves `snapshot`
    /// propagates the error path without panicking.
    struct FailingStorage;

    #[async_trait]
    impl StoragePort for FailingStorage {
        async fn atomic_write(&self, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
            Err(StorageError::NotSupported(
                "snapshot test: atomic_write disabled",
            ))
        }
        async fn vector_search(
            &self,
            _index: &str,
            _query: &[f32],
            _k: usize,
            _filter: Option<&Filter>,
            _as_of: Option<Hlc>,
            _rerank: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            Err(StorageError::NotSupported("FailingStorage::vector_search"))
        }
        async fn graph_traverse(
            &self,
            _query: &CypherQuery,
            _as_of: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            Err(StorageError::NotSupported("FailingStorage::graph_traverse"))
        }
        async fn scan_range(
            &self,
            _prefix: &[u8],
            _as_of: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            Err(StorageError::NotSupported("FailingStorage::scan_range"))
        }
        async fn read_as_of(
            &self,
            _key: &[u8],
            _as_of: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            Err(StorageError::NotSupported("FailingStorage::read_as_of"))
        }
        async fn publish(
            &self,
            _topic: &str,
            _partition: u16,
            _payload: Bytes,
        ) -> Result<u64, StorageError> {
            Err(StorageError::NotSupported("FailingStorage::publish"))
        }
        async fn subscribe(
            &self,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            Err(StorageError::NotSupported("FailingStorage::subscribe"))
        }
        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities {
                bi_temporal_native: false,
                graph_native: false,
                rerank_native: false,
                queue_native: false,
                max_vector_dim: 768,
                native_rrf: false,
            }
        }
    }

    /// Plan 08-00 Task 1 — Test 1.
    ///
    /// `snapshot()` returns an `Ok(Lsn)` and two sequential calls on the same
    /// handle observe strictly monotonic LSNs.
    #[tokio::test]
    async fn test_snapshot_returns_monotonic_lsn() {
        let storage: Arc<dyn StoragePort> = Arc::new(MonotonicLsnStorage::default());
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
        let clock = HlcClock::new(0);
        let handle = Lunaris::with_parts(storage, embedder, clock);

        let a = handle.snapshot().await.expect("first snapshot");
        let b = handle.snapshot().await.expect("second snapshot");
        assert!(b > a, "expected monotonic LSN: a={a:?} b={b:?}");
    }

    /// Plan 08-00 Task 1 — Test 2.
    ///
    /// `snapshot()` propagates `StorageError` through the `#[from]`-derived
    /// `LunarisError::Storage` variant without panicking.
    #[tokio::test]
    async fn test_snapshot_fallible() {
        let storage: Arc<dyn StoragePort> = Arc::new(FailingStorage);
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
        let clock = HlcClock::new(0);
        let handle = Lunaris::with_parts(storage, embedder, clock);

        let r = handle.snapshot().await;
        assert!(
            matches!(r, Err(LunarisError::Storage(_))),
            "expected LunarisError::Storage(_); got {r:?}"
        );
    }
}
