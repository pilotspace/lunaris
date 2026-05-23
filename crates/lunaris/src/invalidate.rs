//! `Lunaris::invalidate_range` — bulk FT invalidation for Helios force-push recovery.
//!
//! ## Design rationale
//!
//! When `helios-git` detects a force-push or rebase, it calls
//! `Lunaris::invalidate_range(scope, node_id, hlc_lo, hlc_hi)` to evict stale
//! recall from the agent's memory. This module implements the fan-out: it issues
//! `FT.INVALIDATE_RANGE` against every known Lunaris collection in parallel and
//! aggregates the delete counts.
//!
//! ## Collections targeted
//!
//! Lunaris manages four FT indices per scope on Moon:
//! `chunks`, `entities`, `facts`, `communities`.
//!
//! Note: `helios-index` may add additional indices (`symbols`, `commits`) on top
//! of these; those are NOT managed by this module. See SUMMARY for the schema
//! roadmap.
//!
//! ## Degraded mode
//!
//! If a collection's index is missing (`WRONGTYPE` from Moon — index does not exist)
//! or the backend does not implement the primitive (`NotSupported`), that collection
//! is skipped with a `WARN` log. The method still returns `Ok(partial_count)`.
//!
//! ## IO failure surface (CONTEXT.md §5)
//!
//! | Surface          | Timeout | Retry | Fallback                          |
//! |------------------|---------|-------|-----------------------------------|
//! | Per-index call   | 250 ms  | none  | warn + skip (degraded mode count) |
//!
//! ## Closed-interval semantics
//!
//! Moon's `FT.INVALIDATE_RANGE` uses a closed `[lo, hi]` interval. Both
//! `hlc_wall_lo_inclusive` and `hlc_wall_hi_inclusive` are inclusive millisecond
//! timestamps. Callers with half-open ranges (`lo..hi`) must pass `hi - 1`.

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use lunaris_core::error::LunarisError;
use lunaris_core::{Scope, StoragePort};
use tracing::{instrument, warn};

/// Known Lunaris FT index kinds. These map to Moon indices via
/// `lunaris_storage_moon::keyspace::ft_index_name(scope, kind)`.
///
/// Note: `symbols` and `commits` are added by `helios-index` and are NOT
/// present in this list — they require schema additions not yet landed.
/// See `.planning/W2-L2-INVALIDATE-RANGE-SUMMARY.md`.
const KNOWN_COLLECTIONS: &[&str] = &["chunks", "entities", "facts", "communities"];

/// Field name for the node-id TAG field expected in FT index schemas.
///
/// ## Schema precondition
///
/// This field must be declared as `TAG` in the `FT.CREATE` schema. Indices
/// predating the `helios-git` schema additions (see SUMMARY) lack it and
/// silently return 0 (Moon bitmap intersect is empty — not an error).
pub(crate) const HLC_NODE_ID_FIELD: &str = "hlc_node_id";

/// Field name for the HLC wall-clock NUMERIC field expected in FT index schemas.
///
/// ## Schema precondition
///
/// This field must be declared as `NUMERIC` in the `FT.CREATE` schema. Indices
/// predating the `helios-git` schema additions silently return 0.
pub(crate) const HLC_WALL_FIELD: &str = "hlc_wall";

/// Per-index call timeout (CONTEXT.md §5 IO failure surface).
const PER_INDEX_TIMEOUT: Duration = Duration::from_millis(250);

/// Bulk-invalidate FT index records authored by `node_id` within
/// `[hlc_wall_lo_inclusive, hlc_wall_hi_inclusive]` across all known collections.
///
/// Returns the total count of invalidated records. Individual collection errors
/// (missing index, unsupported backend) are warned and skipped (degraded mode).
#[instrument(skip(storage), fields(node_id, hlc_wall_lo_inclusive, hlc_wall_hi_inclusive))]
pub(crate) async fn invalidate_range(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
    node_id: &str,
    hlc_wall_lo_inclusive: i64,
    hlc_wall_hi_inclusive: i64,
) -> Result<u64, LunarisError> {
    // Short-circuit: empty or inverted range — no wire calls needed.
    if hlc_wall_lo_inclusive > hlc_wall_hi_inclusive {
        return Ok(0);
    }

    // Fan-out: issue FT.INVALIDATE_RANGE for each collection in parallel.
    // We use `join_all` (not `try_join_all`) so a missing index on one
    // collection does NOT abort the others — degraded mode continues.
    let futs: Vec<_> = KNOWN_COLLECTIONS
        .iter()
        .map(|&kind| {
            let storage = Arc::clone(storage);
            let scope = scope.clone();
            let node_id = node_id.to_owned();
            async move {
                let result = tokio::time::timeout(
                    PER_INDEX_TIMEOUT,
                    storage.invalidate_range(
                        &scope,
                        kind,
                        HLC_NODE_ID_FIELD,
                        &node_id,
                        HLC_WALL_FIELD,
                        hlc_wall_lo_inclusive,
                        hlc_wall_hi_inclusive,
                    ),
                )
                .await;

                match result {
                    // Timeout — warn and skip.
                    Err(_elapsed) => {
                        warn!(
                            collection = kind,
                            timeout_ms = PER_INDEX_TIMEOUT.as_millis(),
                            "invalidate_range: per-index call timed out (degraded mode)"
                        );
                        0u64
                    }
                    // Backend error — check for degraded-mode cases.
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        // WRONGTYPE = index does not exist on Moon.
                        // NotSupported = backend (Postgres/Embedded) has no primitive.
                        // Both are expected in degraded mode — warn and skip.
                        if msg.contains("WRONGTYPE")
                            || msg.contains("not supported")
                            || msg.contains("NotSupported")
                            || msg.contains("no such index")
                        {
                            warn!(
                                collection = kind,
                                error = %e,
                                "invalidate_range: index missing or unsupported — skipping (degraded mode)"
                            );
                        } else {
                            warn!(
                                collection = kind,
                                error = %e,
                                "invalidate_range: backend error — skipping (degraded mode)"
                            );
                        }
                        0u64
                    }
                    // Success — collect count.
                    Ok(Ok(count)) => count,
                }
            }
        })
        .collect();

    let counts = join_all(futs).await;
    let total: u64 = counts.into_iter().sum();
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use lunaris_core::storage::capabilities::{CypherDialect, StorageCapabilities};
    use lunaris_core::{
        CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row, StorageError, VectorHit, WriteOp,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::time::Instant;

    // ── Capabilities helper ───────────────────────────────────────────────────

    fn no_capabilities() -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 0,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: CypherDialect::Legacy,
        }
    }

    // ── MockStorage: uniform behavior + shared call counter ───────────────────

    struct MockStorage {
        call_count: Arc<AtomicU64>,
        behavior: MockBehavior,
    }

    enum MockBehavior {
        Returns(u64),
        MissingIndex,
        NotSupported,
        Timeout,
    }

    impl MockStorage {
        fn new(behavior: MockBehavior) -> (Arc<Self>, Arc<AtomicU64>) {
            let counter = Arc::new(AtomicU64::new(0));
            let mock = Arc::new(MockStorage { call_count: Arc::clone(&counter), behavior });
            (mock, counter)
        }
    }

    #[async_trait]
    impl StoragePort for MockStorage {
        async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
            unimplemented!("stub")
        }
        #[allow(clippy::too_many_arguments)]
        async fn vector_search(
            &self,
            _: &Scope,
            _: &str,
            _: &[f32],
            _: usize,
            _: Option<&Filter>,
            _: Option<Hlc>,
            _: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            unimplemented!("stub")
        }
        async fn graph_traverse(
            &self,
            _: &Scope,
            _: &CypherQuery,
            _: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            unimplemented!("stub")
        }
        async fn scan_range(
            &self,
            _: &Scope,
            _: &[u8],
            _: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            unimplemented!("stub")
        }
        async fn read_as_of(
            &self,
            _: &Scope,
            _: &[u8],
            _: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            unimplemented!("stub")
        }
        async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
            unimplemented!("stub")
        }
        async fn subscribe(
            &self,
            _: &Scope,
            _: &str,
            _: &str,
            _: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            unimplemented!("stub")
        }
        fn capabilities(&self) -> StorageCapabilities {
            no_capabilities()
        }

        async fn invalidate_range(
            &self,
            _scope: &Scope,
            _index: &str,
            _node_id_field: &str,
            _node_id_value: &str,
            _hlc_wall_field: &str,
            _lo: i64,
            _hi: i64,
        ) -> Result<u64, StorageError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                MockBehavior::Returns(n) => Ok(n),
                MockBehavior::MissingIndex => {
                    Err(StorageError::Backend("moon: WRONGTYPE no such index".into()))
                }
                MockBehavior::NotSupported => Err(StorageError::NotSupported(
                    "invalidate_range not implemented for this StoragePort backend",
                )),
                MockBehavior::Timeout => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(0)
                }
            }
        }
    }

    // ── PerCollectionMock: different count per index name ─────────────────────

    struct PerCollectionMock;

    #[async_trait]
    impl StoragePort for PerCollectionMock {
        async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
            unimplemented!("stub")
        }
        #[allow(clippy::too_many_arguments)]
        async fn vector_search(
            &self,
            _: &Scope,
            _: &str,
            _: &[f32],
            _: usize,
            _: Option<&Filter>,
            _: Option<Hlc>,
            _: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            unimplemented!("stub")
        }
        async fn graph_traverse(
            &self,
            _: &Scope,
            _: &CypherQuery,
            _: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            unimplemented!("stub")
        }
        async fn scan_range(
            &self,
            _: &Scope,
            _: &[u8],
            _: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            unimplemented!("stub")
        }
        async fn read_as_of(
            &self,
            _: &Scope,
            _: &[u8],
            _: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            unimplemented!("stub")
        }
        async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
            unimplemented!("stub")
        }
        async fn subscribe(
            &self,
            _: &Scope,
            _: &str,
            _: &str,
            _: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            unimplemented!("stub")
        }
        fn capabilities(&self) -> StorageCapabilities {
            no_capabilities()
        }

        async fn invalidate_range(
            &self,
            _scope: &Scope,
            index: &str,
            _node_id_field: &str,
            _node_id_value: &str,
            _hlc_wall_field: &str,
            _lo: i64,
            _hi: i64,
        ) -> Result<u64, StorageError> {
            // chunks=5, entities=3, facts=0, communities=0 → sum=8
            match index {
                "chunks" => Ok(5),
                "entities" => Ok(3),
                _ => Ok(0),
            }
        }
    }

    // ── SlowMock: 100ms sleep per call — proves parallel fan-out ──────────────

    struct SlowMock;

    #[async_trait]
    impl StoragePort for SlowMock {
        async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
            unimplemented!("stub")
        }
        #[allow(clippy::too_many_arguments)]
        async fn vector_search(
            &self,
            _: &Scope,
            _: &str,
            _: &[f32],
            _: usize,
            _: Option<&Filter>,
            _: Option<Hlc>,
            _: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            unimplemented!("stub")
        }
        async fn graph_traverse(
            &self,
            _: &Scope,
            _: &CypherQuery,
            _: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            unimplemented!("stub")
        }
        async fn scan_range(
            &self,
            _: &Scope,
            _: &[u8],
            _: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            unimplemented!("stub")
        }
        async fn read_as_of(
            &self,
            _: &Scope,
            _: &[u8],
            _: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            unimplemented!("stub")
        }
        async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
            unimplemented!("stub")
        }
        async fn subscribe(
            &self,
            _: &Scope,
            _: &str,
            _: &str,
            _: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            unimplemented!("stub")
        }
        fn capabilities(&self) -> StorageCapabilities {
            no_capabilities()
        }

        async fn invalidate_range(
            &self,
            _scope: &Scope,
            _index: &str,
            _node_id_field: &str,
            _node_id_value: &str,
            _hlc_wall_field: &str,
            _lo: i64,
            _hi: i64,
        ) -> Result<u64, StorageError> {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(1)
        }
    }

    // ── Helper ────────────────────────────────────────────────────────────────

    fn test_scope() -> Scope {
        Scope::new("test.worktree-abc").unwrap()
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Fan-out: `invalidate_range` calls the storage for every known collection.
    #[tokio::test]
    async fn invalidate_range_fans_out_to_all_collections() {
        let (mock, counter) = MockStorage::new(MockBehavior::Returns(1));
        let storage: Arc<dyn StoragePort> = mock;
        let scope = test_scope();

        invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_000, 2_000)
            .await
            .expect("should succeed");

        assert_eq!(
            counter.load(Ordering::SeqCst),
            KNOWN_COLLECTIONS.len() as u64,
            "expected one call per collection ({} total)",
            KNOWN_COLLECTIONS.len()
        );
    }

    /// Count aggregation: partial counts from each collection are summed.
    #[tokio::test]
    async fn invalidate_range_aggregates_counts_correctly() {
        let storage: Arc<dyn StoragePort> = Arc::new(PerCollectionMock);
        let scope = test_scope();

        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_000, 2_000)
            .await
            .expect("should succeed");

        // chunks=5, entities=3, facts=0, communities=0 → total=8
        assert_eq!(total, 8, "should sum all collection counts");
    }

    /// Missing-index tolerance: WRONGTYPE errors are warn-and-skipped (degraded mode).
    #[tokio::test]
    async fn invalidate_range_tolerates_missing_index_with_warn() {
        let (mock, _counter) = MockStorage::new(MockBehavior::MissingIndex);
        let storage: Arc<dyn StoragePort> = mock;
        let scope = test_scope();

        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_000, 2_000)
            .await
            .expect("should succeed in degraded mode");

        assert_eq!(total, 0, "missing index should degrade to 0, not error");
    }

    /// NotSupported backend: same degraded-mode behavior as missing index.
    #[tokio::test]
    async fn invalidate_range_tolerates_not_supported_backend() {
        let (mock, _counter) = MockStorage::new(MockBehavior::NotSupported);
        let storage: Arc<dyn StoragePort> = mock;
        let scope = test_scope();

        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_000, 2_000)
            .await
            .expect("degraded mode: NotSupported should not propagate as error");

        assert_eq!(total, 0, "NotSupported should degrade to 0");
    }

    /// Timeout: per-index calls exceeding 250ms are skipped with 0 contribution.
    #[tokio::test]
    async fn invalidate_range_timeout_skips_slow_collections() {
        let (mock, _counter) = MockStorage::new(MockBehavior::Timeout);
        let storage: Arc<dyn StoragePort> = mock;
        let scope = test_scope();

        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_000, 2_000)
            .await
            .expect("timeout should produce Ok(0), not Err");

        assert_eq!(total, 0, "timed-out collections should contribute 0");
    }

    /// Empty range (lo > hi): returns Ok(0) immediately without any wire calls.
    #[tokio::test]
    async fn invalidate_range_empty_range_returns_zero_without_calls() {
        let (mock, counter) = MockStorage::new(MockBehavior::Returns(99));
        let storage: Arc<dyn StoragePort> = mock;
        let scope = test_scope();

        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 2_000, 1_000)
            .await
            .expect("empty range should return Ok(0)");

        assert_eq!(total, 0, "inverted range must return 0");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "empty range must not issue any storage calls"
        );
    }

    /// Equal bounds (lo == hi): valid single-point closed range — calls ARE made.
    #[tokio::test]
    async fn invalidate_range_equal_bounds_is_valid_single_point() {
        let (mock, _counter) = MockStorage::new(MockBehavior::Returns(2));
        let storage: Arc<dyn StoragePort> = mock;
        let scope = test_scope();

        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_500, 1_500)
            .await
            .expect("single-point range should succeed");

        assert_eq!(
            total,
            (KNOWN_COLLECTIONS.len() as u64) * 2,
            "each of {} collections returns 2 → total {}",
            KNOWN_COLLECTIONS.len(),
            KNOWN_COLLECTIONS.len() * 2
        );
    }

    /// Parallel fan-out: all collections run concurrently, not sequentially.
    ///
    /// Proof: 4 collections each sleep 100ms. If parallel, wall time < 200ms.
    /// If sequential, wall time ≥ 400ms.
    #[tokio::test]
    async fn invalidate_range_fan_out_is_parallel() {
        let storage: Arc<dyn StoragePort> = Arc::new(SlowMock);
        let scope = test_scope();

        let t0 = Instant::now();
        let total = invalidate_range(&storage, &scope, "helios-git@aabbcc", 1_000, 2_000)
            .await
            .expect("parallel fan-out should succeed");
        let elapsed = t0.elapsed();

        assert_eq!(total, KNOWN_COLLECTIONS.len() as u64, "all collections contribute 1 each");
        // Parallel: 4 × 100ms calls complete in well under 300ms.
        // Sequential would take ≥ 400ms.
        assert!(
            elapsed < Duration::from_millis(300),
            "fan-out must be parallel: elapsed {}ms ≥ 300ms",
            elapsed.as_millis()
        );
    }
}
