//! ADD task `forget-scope-routing` (contract FROZEN @ v1, 2026-07-14):
//! hydrate/hydrate_mixed must DROP sys-closed rows so a scoped soft-delete
//! (`ScopedLunaris::forget` stamping `bt.sys.1`) is visible to recall.
//!
//! Deep-test evidence (memory `project_lunaris_mcp_deep_test_findings` §1):
//! hydrate today has ZERO sys handling — a soft-deleted episode still
//! hydrates, so even a correctly-scoped forget would not hide content.
//!
//! RED until the sys-gate lands in `crates/lunaris-retrieve/src/hydrate.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::keyspace::{chunk_key, episode_key, fact_key};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Episode, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
};
use lunaris_extract::types::{EntityId, Fact};
use lunaris_retrieve::hydrate::{hydrate, hydrate_mixed};
use lunaris_retrieve::types::{RawHit, SourceOp};
use ulid::Ulid;

// ─── BtKvStorage — HashMap mock with PER-ROW BiTemporal ─────────────────────

struct BtKvStorage {
    rows: HashMap<Vec<u8>, (Vec<u8>, BiTemporal)>,
}

#[async_trait]
impl StoragePort for BtKvStorage {
    async fn read_as_of(
        &self,
        _scope: &Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.get(key).cloned().map(|(v, bt)| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt,
        }))
    }

    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
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
        Err(StorageError::NotSupported("BtKvStorage"))
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("BtKvStorage"))
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(vec![]).boxed())
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("BtKvStorage"))
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("BtKvStorage"))
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
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for BtKvStorage {
    async fn keyword_search(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(vec![])
    }
}

// ─── Fixture ─────────────────────────────────────────────────────────────────

fn raw(id: Ulid) -> RawHit {
    RawHit {
        id: id.to_bytes().to_vec(),
        score: 0.9,
        rerank_applied: false,
        degraded: false,
        metadata: serde_json::json!({}),
        source_op: SourceOp::Vector,
    }
}

fn open_bt() -> BiTemporal {
    BiTemporal::at(Hlc::ZERO, Hlc::ZERO)
}

fn closed_bt() -> BiTemporal {
    let mut bt = BiTemporal::at(Hlc::ZERO, Hlc::ZERO);
    bt.invalidate_sys(Hlc { wall_ms: 42, counter: 0, node_id: 0 });
    bt
}

/// Seed two (episode, chunk) pairs under `scope`. Episode A is sys-closed
/// (payload bt stamped via `invalidate_sys` — the exact mutation
/// `build_soft_delete_op` persists); episode B stays sys-open.
/// Returns (storage, chunk_a_id, chunk_b_id).
fn seeded_pair(scope: &Scope) -> (Arc<BtKvStorage>, Ulid, Ulid) {
    use lunaris_core::primitives::Chunk;
    let clock = HlcClock::new(0);

    let mut ep_a = Episode::new(scope.clone(), "forgotten:src", "forgotten content", &clock);
    ep_a.bt.invalidate_sys(clock.tick());
    let chunk_a = Chunk::new(scope.clone(), ep_a.id, "forgotten chunk text", 3, 0, vec![], &clock);

    let ep_b = Episode::new(scope.clone(), "kept:src", "kept content", &clock);
    let chunk_b = Chunk::new(scope.clone(), ep_b.id, "kept chunk text", 3, 0, vec![], &clock);

    let (a_id, b_id) = (chunk_a.id, chunk_b.id);
    let mut rows = HashMap::new();
    rows.insert(episode_key(scope, ep_a.id), (serde_json::to_vec(&ep_a).unwrap(), closed_bt()));
    rows.insert(chunk_key(scope, a_id), (serde_json::to_vec(&chunk_a).unwrap(), open_bt()));
    rows.insert(episode_key(scope, ep_b.id), (serde_json::to_vec(&ep_b).unwrap(), open_bt()));
    rows.insert(chunk_key(scope, b_id), (serde_json::to_vec(&chunk_b).unwrap(), open_bt()));
    (Arc::new(BtKvStorage { rows }), a_id, b_id)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// §2 "sys-closed row dropped at hydrate": a chunk whose PARENT EPISODE is
/// sys-closed must not hydrate; the sys-open sibling hydrates unchanged.
#[tokio::test]
async fn hydrate_drops_chunk_of_sys_closed_episode_keeps_open_sibling() {
    let scope = Scope::new("sys-gate-a").unwrap();
    let (storage, chunk_a, chunk_b) = seeded_pair(&scope);

    let hits = hydrate(storage.as_ref(), &scope, vec![raw(chunk_a), raw(chunk_b)], None, false)
        .await
        .expect("hydrate must succeed");

    assert_eq!(hits.len(), 1, "sys-closed episode's chunk must be dropped, got {hits:?}");
    assert_eq!(hits[0].text, "kept chunk text");
    assert_eq!(hits[0].source, "kept:src");
}

/// A chunk row that is ITSELF sys-closed (payload bt stamped) must not
/// hydrate even when its parent episode is open.
#[tokio::test]
async fn hydrate_drops_sys_closed_chunk_row() {
    use lunaris_core::primitives::Chunk;
    let scope = Scope::new("sys-gate-b").unwrap();
    let clock = HlcClock::new(0);

    let ep = Episode::new(scope.clone(), "open:src", "open content", &clock);
    let mut chunk = Chunk::new(scope.clone(), ep.id, "self-closed chunk", 3, 0, vec![], &clock);
    chunk.bt.invalidate_sys(clock.tick());
    let chunk_id = chunk.id;

    let mut rows = HashMap::new();
    rows.insert(episode_key(&scope, ep.id), (serde_json::to_vec(&ep).unwrap(), open_bt()));
    rows.insert(chunk_key(&scope, chunk_id), (serde_json::to_vec(&chunk).unwrap(), closed_bt()));
    let storage = Arc::new(BtKvStorage { rows });

    let hits = hydrate(storage.as_ref(), &scope, vec![raw(chunk_id)], None, false)
        .await
        .expect("hydrate must succeed");
    assert!(hits.is_empty(), "sys-closed chunk row must be dropped, got {hits:?}");
}

/// hydrate_mixed: a FACT row whose KV Row bt is sys-closed must be dropped;
/// the sys-open fact hydrates unchanged (gate rides the Row bt because the
/// at-rest `lunaris_extract::Fact` payload carries no `bt`).
#[tokio::test]
async fn hydrate_mixed_drops_sys_closed_fact_row() {
    let scope = Scope::new("sys-gate-c").unwrap();

    let dead_id = Ulid::new();
    let live_id = Ulid::new();
    let mk = |id: Ulid, text: &str| Fact {
        id,
        subject_id: EntityId([1u8; 16]),
        predicate: "listens_on".to_owned(),
        object_id: EntityId([2u8; 16]),
        fact_text: text.to_owned(),
        confidence: 0.9,
        valid_from_iso: "2026-07-14T00:00:00Z".to_owned(),
        valid_to_iso: None,
    };

    let mut rows = HashMap::new();
    rows.insert(
        fact_key(&scope, dead_id),
        (serde_json::to_vec(&mk(dead_id, "forgotten fact")).unwrap(), closed_bt()),
    );
    rows.insert(
        fact_key(&scope, live_id),
        (serde_json::to_vec(&mk(live_id, "kept fact")).unwrap(), open_bt()),
    );
    let storage = Arc::new(BtKvStorage { rows });

    let hits =
        hydrate_mixed(storage.as_ref(), &scope, vec![raw(dead_id), raw(live_id)], None, false)
            .await
            .expect("hydrate_mixed must succeed");

    assert_eq!(hits.len(), 1, "sys-closed fact row must be dropped, got {hits:?}");
    assert_eq!(hits[0].text, "kept fact");
}
