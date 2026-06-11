//! T1 regression guard (260609-dvi): archive_event returns ConsolidatorArchive;
//! epoch-aged ConsolidateEvent archives under ActR scoring.
//!
//! Two assertions:
//! (a) `lunaris_consolidate::archive_event` returns `AuditEvent::ConsolidatorArchive`.
//!     This is a compile + correctness guard: the function was `pub(crate)` before
//!     T1 and could not be called from here. It also verifies the correct variant.
//! (b) ActRConsolidator archives an event with `lsn_wall_ms=1000` (near epoch).
//!     Near-epoch → huge elapsed → activation deeply negative → `should_archive` fires.
//!     Regression guard: consolidate() must produce a non-empty archives vec.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_consolidate::{ActRConsolidator, ConsolidateEvent, Consolidator};
use lunaris_core::{CypherDialect, QueueMsg, StorageCapabilities, StorageError, StoragePort};
use ulid::Ulid;

// ── NullStorage fixture ───────────────────────────────────────────────────────

/// Minimal StoragePort that returns empty/Ok for every method.
/// ActRConsolidator only needs `atomic_write` during consolidation (to persist
/// the FactId row); we return Ok(Lsn{0,0}) so it succeeds without a real DB.
#[derive(Default)]
struct NullStorage;

#[async_trait]
impl StoragePort for NullStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        _ops: &[lunaris_core::WriteOp],
    ) -> Result<lunaris_core::Lsn, StorageError> {
        Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
    }

    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&lunaris_core::Filter>,
        _as_of: Option<lunaris_core::Hlc>,
        _rerank: bool,
    ) -> Result<Vec<lunaris_core::VectorHit>, StorageError> {
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _query: &lunaris_core::CypherQuery,
        _as_of: Option<lunaris_core::Hlc>,
    ) -> Result<lunaris_core::GraphResult, StorageError> {
        Ok(lunaris_core::GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<lunaris_core::Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(futures::stream::iter(Vec::new())))
    }

    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        _key: &[u8],
        _as_of: lunaris_core::Hlc,
    ) -> Result<Option<lunaris_core::Row<Bytes>>, StorageError> {
        Ok(None)
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// (a) `archive_event` returns `AuditEvent::ConsolidatorArchive`.
///
/// Before T1, `archive_event` was `pub(crate)` and unreachable from here.
/// This test proves the fn is `pub` AND returns the correct variant.
#[test]
fn archive_event_returns_consolidator_archive_variant() {
    use lunaris_consolidate::archive_event;
    use lunaris_consolidate::types::ArchiveEvent;
    use lunaris_consolidate::types::FactId;
    use lunaris_core::audit::AuditEvent;

    let ae = archive_event(&ArchiveEvent {
        fact_id: FactId(Ulid::new().to_bytes()),
        final_activation: -3.5,
        moved_to: "archive".into(),
    });

    assert!(
        matches!(ae, AuditEvent::ConsolidatorArchive { .. }),
        "archive_event must return AuditEvent::ConsolidatorArchive; got: {ae:?}"
    );
}

/// (b) ActRConsolidator archives an epoch-aged event.
///
/// `lsn_wall_ms = 1000` (1970-01-01 + 1s) → huge elapsed relative to now
/// → activation deeply negative → `should_archive` fires.
///
/// This guards the consolidate() → archive path so we know the WIRED test
/// (embedded-moon) will produce non-empty reports when seeded with epoch-aged events.
#[tokio::test]
async fn actr_archives_epoch_aged_event() {
    let consolidator = ActRConsolidator::default();
    let storage: Arc<dyn StoragePort> = Arc::new(NullStorage);

    let event = ConsolidateEvent {
        kind: "ingest_committed".into(),
        episode_id: Ulid::new(),
        lsn_wall_ms: 1000, // near epoch → huge elapsed → archive fires
        lsn_counter: 1,
        source: "scratchpad/test".into(),
    };

    let report = consolidator
        .consolidate(storage, &[event])
        .await
        .expect("consolidate must not error on NullStorage");

    assert!(
        !report.archives.is_empty(),
        "epoch-aged event (lsn_wall_ms=1000) must be archived under ActR; \
         got archives={}, promotions={}",
        report.archives.len(),
        report.promotions.len()
    );
}
