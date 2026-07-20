//! Red/green regression for the `memory.recall` `source_prefix` filter bug
//! (live-reproduced 2026-07-20, scope `git_487b86f2…`, PR follow-up).
//!
//! ## The bug
//!
//! `recall::handle` pushed `source_prefix` DOWN into the storage branches as a
//! `Query::filter = Filter::StartsWith { field: "source", .. }`. On Moon that
//! push-down silently DROPS every match:
//!
//!  - the keyword branch renders `@source:decision:*`, but `source` is a TAG
//!    field (`@source:{decision\:*}` is the only valid syntax) — and on indexes
//!    created before the `source` TAG was added (PERF-MOON-01) the field does
//!    not exist at all, so the FT query matches nothing;
//!  - the vector branch takes an over-fetch + client post-filter path that
//!    returns empty on large/quantized indexes.
//!
//! The handler ALREADY enforces the prefix authoritatively AFTER recall, on the
//! hydrated episode `source` (`h.source.starts_with(prefix)`), backed by an 8×
//! widened candidate window. The push-down is therefore redundant AND
//! destructive: a backend that HONORS it but implements it lossily returns zero
//! candidates, so the authoritative post-filter never sees the match.
//!
//! Live proof: on the failing scope, the SAME candidate set (`candidate_k=40`)
//! returned 0 hits WITH the push-down but 22 matching hits when post-filtered on
//! the hydrated source instead.
//!
//! ## This test
//!
//! `FilterDroppingStorage` models Moon exactly: `vector_search` /
//! `keyword_search` return real hits when NO filter is pushed, and EMPTY the
//! moment a filter is present. Two episodes are ingested — one `decision:`-
//! sourced, one `other:`-sourced — both matching the query. A `source_prefix:
//! "decision:"` recall MUST return the decision episode. It can only do so if
//! the handler does NOT push the filter into storage (RED before the fix, GREEN
//! after).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{EpisodeBuilder, Lunaris};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_memory_service::recall::{RecallFilters, RecallParams, handle};
use parking_lot::Mutex;
use serde_json::json;

/// Storage double that reproduces Moon's lossy filtered-search behaviour: it
/// serves canned vector + keyword hits and hydrates chunk→episode source, but
/// returns EMPTY from either search branch as soon as a `Filter` is pushed
/// down. This is the exact contract the handler must NOT depend on.
#[derive(Default)]
struct FilterDroppingStorage {
    /// key bytes -> serialized JSON value (episodes + chunks).
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// chunk ULID bytes, in insertion order (vector-hit reconstruction).
    chunk_ids: Mutex<Vec<Vec<u8>>>,
}

impl FilterDroppingStorage {
    fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StoragePort for FilterDroppingStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
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
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        // Moon-like: a pushed-down source filter silently drops every candidate.
        if filter.is_some() {
            return Ok(vec![]);
        }
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
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("graph_traverse"))
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
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
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("publish"))
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
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
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for FilterDroppingStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        // Moon-like: a pushed-down source filter silently drops every candidate.
        if filter.is_some() {
            return Ok(vec![]);
        }
        let q = query.to_lowercase();
        let mut hits: Vec<KeywordHit> = Vec::new();
        let rows = self.rows.lock().clone();
        for (key, value) in rows {
            if !key.starts_with(b"lunaris:chunk:") {
                continue;
            }
            let s = String::from_utf8_lossy(&value).to_lowercase();
            if s.contains(&q) {
                let prefix_len = b"lunaris:chunk:".len();
                if let Ok(ulid_str) = std::str::from_utf8(&key[prefix_len..])
                    && let Ok(ulid) = ulid::Ulid::from_string(ulid_str)
                {
                    hits.push(KeywordHit::new(ulid.to_bytes().to_vec(), 0.9, 0.9, json!({})));
                }
            }
            if hits.len() >= k {
                break;
            }
        }
        Ok(hits)
    }
}

/// RED before the fix / GREEN after: a `source_prefix` recall must return the
/// matching episode even when the backend drops every pushed-down filter. The
/// handler achieves this by NOT pushing the prefix into storage and enforcing
/// it on the hydrated `source` instead.
#[tokio::test]
async fn source_prefix_recall_survives_backend_that_drops_pushed_filters() {
    let rec = Arc::new(FilterDroppingStorage::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let lunaris = Lunaris::with_parts_keyword(storage, keyword, embedder, clock);
    let scope = Scope::new("test-recall-moonlike-filter").unwrap();
    let scoped = lunaris.scoped(scope.clone());

    // Two competing candidates, identical text so both are strong recall hits;
    // only the `source` differs.
    scoped
        .ingest(EpisodeBuilder::new(
            "decision:test-moonlike",
            "widget alpha configuration knobs and defaults",
        ))
        .await
        .unwrap();
    scoped
        .ingest(EpisodeBuilder::new(
            "other:test-moonlike",
            "widget alpha configuration knobs and defaults",
        ))
        .await
        .unwrap();

    let resp = handle(
        &lunaris,
        &scope,
        RecallParams {
            query: "widget configuration knobs".into(),
            k: 5,
            filters: Some(RecallFilters { source_prefix: Some("decision:".into()) }),
            as_of: None,
            raw: false,
        },
    )
    .await
    .expect("recall must succeed");

    assert!(
        !resp.hits.is_empty(),
        "source_prefix recall returned ZERO hits — the handler pushed the prefix \
         into the backend, which dropped every match (the live git_487b86f2 bug). \
         It must enforce the prefix on the hydrated source instead."
    );
    assert!(
        resp.hits.iter().all(|h| h.source.starts_with("decision:")),
        "every hit must match the source_prefix; got sources: {:?}",
        resp.hits.iter().map(|h| &h.source).collect::<Vec<_>>()
    );
}

/// Guard the exclusion half: the non-matching `other:` episode must NOT leak
/// through the post-filter (proves we did not simply drop filtering entirely).
#[tokio::test]
async fn source_prefix_recall_still_excludes_non_matching_sources() {
    let rec = Arc::new(FilterDroppingStorage::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let lunaris = Lunaris::with_parts_keyword(storage, keyword, embedder, clock);
    let scope = Scope::new("test-recall-moonlike-excl").unwrap();
    let scoped = lunaris.scoped(scope.clone());

    scoped
        .ingest(EpisodeBuilder::new("other:x", "widget alpha configuration knobs"))
        .await
        .unwrap();

    let resp = handle(
        &lunaris,
        &scope,
        RecallParams {
            query: "widget configuration knobs".into(),
            k: 5,
            filters: Some(RecallFilters { source_prefix: Some("decision:".into()) }),
            as_of: None,
            raw: false,
        },
    )
    .await
    .expect("recall must succeed");

    assert!(
        resp.hits.is_empty(),
        "only an `other:`-sourced episode exists; a decision: prefix must exclude it, \
         got sources: {:?}",
        resp.hits.iter().map(|h| &h.source).collect::<Vec<_>>()
    );
}
