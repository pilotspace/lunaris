//! ADD task `session-start-digest` — engine primitive `recent_by_source`.
//!
//! The SessionStart digest needs the scope's MOST-RECENT durable decisions,
//! ordered by recency and filtered by source prefix — RecallForPrompt has no
//! source filter, and a semantic query would bias to similarity, not recency.
//! `recent_by_source` scans only the scope's `episode:` partition, keeps rows
//! whose `source` starts_with any requested prefix, sorts by ULID (time) DESC,
//! and takes `limit`. Unparseable rows are skipped; an empty scan is Ok(vec![]).

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris::recent_by_source;
use lunaris_core::{
    BiTemporal, CypherDialect, Episode, Hlc, QueueMsg, Scope, StorageCapabilities, StorageError,
    StoragePort, keyspace,
};
use ulid::Ulid;

/// In-memory StoragePort whose `scan_range` replays a seeded key/value set,
/// filtered by the requested prefix (faithful to the scoped-prefix contract).
struct ScanStubStorage {
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    fail_scan: bool,
}

impl ScanStubStorage {
    fn new(rows: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        Self { rows, fail_scan: false }
    }
    fn failing() -> Self {
        Self { rows: Vec::new(), fail_scan: true }
    }
}

#[async_trait]
impl StoragePort for ScanStubStorage {
    async fn atomic_write(
        &self,
        _scope: &Scope,
        _ops: &[lunaris_core::WriteOp],
    ) -> Result<lunaris_core::Lsn, StorageError> {
        Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
    }

    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&lunaris_core::Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<lunaris_core::VectorHit>, StorageError> {
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _query: &lunaris_core::CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<lunaris_core::GraphResult, StorageError> {
        Ok(lunaris_core::GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        if self.fail_scan {
            return Err(StorageError::Backend("scan boom".into()));
        }
        let prefix = prefix.to_vec();
        let hits: Vec<Result<(Bytes, Bytes), StorageError>> = self
            .rows
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| Ok((Bytes::from(k.clone()), Bytes::from(v.clone()))))
            .collect();
        Ok(Box::pin(futures::stream::iter(hits)))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<lunaris_core::Row<Bytes>>, StorageError> {
        Ok(None)
    }

    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
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

fn scope() -> Scope {
    Scope::new("digest_test").expect("valid scope")
}

/// Build an episode row keyed by the scoped `episode:{ulid}` key.
fn episode_row(scope: &Scope, id: Ulid, source: &str, content: &str) -> (Vec<u8>, Vec<u8>) {
    let ep = Episode {
        id,
        scope: scope.clone(),
        source: source.to_owned(),
        content: content.to_owned(),
        t_ref: None,
        bt: BiTemporal { valid: (Hlc::ZERO, None), sys: (Hlc::ZERO, None) },
        metadata: serde_json::Map::new(),
    };
    let key = keyspace::episode_key(scope, id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");
    (key, value)
}

fn ulid_at(ms: u64) -> Ulid {
    Ulid::from_parts(ms, ms as u128)
}

#[tokio::test]
async fn recency_order_keeps_most_recent_decisions() {
    let s = scope();
    let d1 = ulid_at(1000);
    let d2 = ulid_at(2000);
    let d3 = ulid_at(3000);
    let other = ulid_at(4000); // newest, but NOT a decision
    let storage = ScanStubStorage::new(vec![
        episode_row(&s, d1, "decision:proj", "d1 body"),
        episode_row(&s, d2, "decision:proj", "d2 body"),
        episode_row(&s, d3, "decision:proj", "d3 body"),
        episode_row(&s, other, "codex:tool_call:post", "noise"),
    ]);

    let out = recent_by_source(&storage, &s, &["decision:".to_owned()], 2).await.expect("scan ok");

    let ids: Vec<Ulid> = out.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![d3, d2], "must return the 2 most-recent decisions, newest first");
    assert!(out.iter().all(|e| e.source.starts_with("decision:")));
}

#[tokio::test]
async fn source_filter_excludes_non_matches() {
    let s = scope();
    let storage = ScanStubStorage::new(vec![
        episode_row(&s, ulid_at(1000), "decision:x", "keep me"),
        episode_row(&s, ulid_at(2000), "edit:y", "drop me"),
    ]);
    let out = recent_by_source(&storage, &s, &["decision:".to_owned()], 10).await.expect("scan ok");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].content, "keep me");
}

#[tokio::test]
async fn skips_unparseable_row() {
    let s = scope();
    let mut rows = vec![
        episode_row(&s, ulid_at(1000), "decision:a", "valid-a"),
        episode_row(&s, ulid_at(2000), "decision:b", "valid-b"),
    ];
    // a garbage value under a scoped episode key
    rows.push((keyspace::episode_key(&s, ulid_at(3000)), b"{not json".to_vec()));
    let storage = ScanStubStorage::new(rows);
    let out = recent_by_source(&storage, &s, &["decision:".to_owned()], 10).await.expect("scan ok");
    assert_eq!(out.len(), 2, "garbage row skipped, valid decisions kept");
}

#[tokio::test]
async fn empty_scope_returns_empty_vec() {
    let s = scope();
    let storage = ScanStubStorage::new(Vec::new());
    let out = recent_by_source(&storage, &s, &["decision:".to_owned()], 5).await.expect("scan ok");
    assert!(out.is_empty());
}

#[tokio::test]
async fn scan_error_propagates_as_err() {
    let s = scope();
    let storage = ScanStubStorage::failing();
    let out = recent_by_source(&storage, &s, &["decision:".to_owned()], 5).await;
    assert!(out.is_err(), "scan failure must propagate so the caller can fail-to-empty");
}
