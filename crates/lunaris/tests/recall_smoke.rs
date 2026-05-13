//! End-to-end smoke test: ingest one Episode, recall via the DSL,
//! assert hit text + source come back.
//!
//! Uses an in-memory `RecordingStorage` that ALSO impls `KeywordPort` so the
//! umbrella `Lunaris::recall()` builder can dispatch to either path. The
//! storage stores chunks in a `HashMap` keyed by the chunk lookup key
//! (`lunaris:chunk:<ulid>`) and serves them back via `read_as_of`. Vector
//! search returns canned hits matching the chunks' ids.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Lunaris, Query, Vector};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_core::{Chunk, Episode};
use parking_lot::Mutex;
use serde_json::json;

/// In-memory storage that records writes AND serves chunks back via
/// `read_as_of`. Mirrors the Phase 2 ingest pipeline keyspace
/// (`lunaris:chunk:<ulid>` for chunks, `lunaris:episode:<ulid>` for episodes).
#[derive(Default)]
struct RecordingStorageWithKeyword {
    /// key bytes -> serialized JSON value
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// ULID id bytes -> chunk ULID bytes (for vector search hit reconstruction)
    chunk_ids: Mutex<Vec<Vec<u8>>>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    self.rows.lock().insert(key.clone(), value.clone());
                }
                WriteOp::VectorUpsert { id, .. } => {
                    self.chunk_ids.lock().push(id.clone());
                }
                _ => {}
            }
        }
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }

    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        let ids = self.chunk_ids.lock().clone();
        Ok(ids
            .into_iter()
            .take(k)
            .enumerate()
            .map(|(i, id)| VectorHit {
                id,
                score: 1.0 - (i as f32 * 0.1),
                rerank_applied: false,
                metadata: json!({}),
            })
            .collect())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("graph_traverse"))
    }

    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }

    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("publish"))
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("subscribe"))
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
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorageWithKeyword {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        query: &str,
        k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        // Return chunks whose serialized JSON contains the query word as a hit.
        let q = query.to_lowercase();
        let mut hits: Vec<KeywordHit> = Vec::new();
        let rows = self.rows.lock().clone();
        for (key, value) in rows {
            // Match only on chunk rows (not episode rows).
            if !key.starts_with(b"lunaris:chunk:") {
                continue;
            }
            let s = String::from_utf8_lossy(&value).to_lowercase();
            if s.contains(&q) {
                // Recover the chunk ULID bytes from the key.
                let prefix_len = b"lunaris:chunk:".len();
                if let Ok(ulid_str) = std::str::from_utf8(&key[prefix_len..])
                    && let Ok(ulid) = ulid::Ulid::from_string(ulid_str)
                {
                    let id_bytes = ulid.to_bytes().to_vec();
                    hits.push(KeywordHit::new(id_bytes, 0.9, 0.9, json!({})));
                }
            }
            if hits.len() >= k {
                break;
            }
        }
        Ok(hits)
    }
}

#[tokio::test]
async fn ingest_then_recall_returns_hit() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone());

    // Ingest one Episode.
    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "notes.md",
        "# Notes\nThe quick brown fox jumps over the lazy dog.",
        &clock,
    );
    let lsn = handle.ingest(ep).await.expect("ingest must succeed");
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "lsn must be non-zero");

    // Recall via the DSL — vector-only over chunks, top 3.
    let hits = handle
        .recall()
        .with_root(Vector::new("chunks", 30).top(3))
        .execute(Query::text("brown fox"))
        .await
        .expect("recall must succeed");

    assert!(!hits.is_empty(), "at least one hit must come back");
    // Confirm the hit text + source were hydrated end-to-end.
    let first = &hits[0];
    assert!(
        first.text.to_lowercase().contains("brown fox"),
        "first hit text must contain the search term: got `{}`",
        first.text
    );
    assert_eq!(first.source, "notes.md");
}

#[tokio::test]
async fn recall_returns_empty_when_no_chunks_exist() {
    // Sanity: no ingest → no chunk rows → recall returns empty.
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock);

    let hits = handle
        .recall()
        .with_root(Vector::new("chunks", 30))
        .execute(Query::text("nothing here"))
        .await
        .expect("recall on empty store must succeed");
    assert!(hits.is_empty());
}

// Use Chunk type so the unused-import lint stays quiet (Chunk is imported for
// the smoke test's domain semantics; the actual chunk wiring is internal to
// the Phase 2 ingest pipeline so we don't construct one directly here).
#[allow(dead_code)]
fn _chunk_marker(c: Chunk) -> Chunk {
    c
}
