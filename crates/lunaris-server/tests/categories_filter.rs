//! RED suite for `multi-level-memory-categories` — categories become a
//! storage `Filter` on the recall path, AND-combined with the string DSL.
//!
//! RED reason (runtime, the right reason): `RecallRequest` does not yet carry
//! `categories`, so `deny_unknown_fields` rejects these bodies with 422 and
//! `vector_search` is never reached — the captured filter stays `None`. After
//! BUILD wires the DTO + handler, recall composes a `Filter::Eq`/`Or` on the
//! `categories` field and AND-combines it with any string-DSL filter, so the
//! capture stub observes it.
//!
//! The stub honors nothing semantically (returns zero hits); it only RECORDS
//! the `Filter` that the handler pushed down. Honoring the filter is the
//! backend's job and is covered by the Moon/Postgres conformance suites — the
//! discriminating fact here is that the wire `categories` reach the storage
//! filter at all.

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::Lunaris;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use parking_lot::Mutex;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixture — captures the Filter pushed into vector_search (as JSON).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CaptureStorage {
    captured_filter: Mutex<Option<serde_json::Value>>,
    vector_search_calls: Mutex<usize>,
}

#[async_trait]
impl StoragePort for CaptureStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }

    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        *self.vector_search_calls.lock() += 1;
        if let Some(f) = filter {
            *self.captured_filter.lock() =
                Some(serde_json::to_value(f).expect("Filter serializes"));
        }
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
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
        Ok(Box::pin(stream::empty()))
    }

    async fn queue_depth(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
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
impl KeywordPort for CaptureStorage {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_tokens() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-cats-tokens-{}.json", ulid::Ulid::new()));
    let body = serde_json::json!({
        "tok-a": { "tenant": "agent-a", "scopes": ["ingest", "recall", "forget"] },
    });
    std::fs::write(&path, body.to_string()).expect("write tokens file");
    path
}

fn build_app() -> (axum::Router, Arc<CaptureStorage>) {
    let storage = Arc::new(CaptureStorage::default());
    let lunaris = Arc::new(Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage.clone() as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ));
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file: write_tokens(),
        rate_per_second: 1000,
        rate_burst: 1000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        http_timeout_secs: 30,
        http_concurrency: 256,
        metrics_disabled: false,
    };
    (lunaris_server::build(cfg, lunaris), storage)
}

fn recall_request(body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("recall req")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A `categories` list on recall must reach the storage layer as a `Filter`
/// referencing the `categories` field and the requested value.
#[tokio::test]
async fn categories_become_storage_filter() {
    let (app, storage) = build_app();

    let body = serde_json::json!({
        "query": "what do we know",
        "k": 5,
        "categories": ["urgent"],
    });
    let resp = app.oneshot(recall_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "valid categories recall must succeed");

    let captured = storage.captured_filter.lock().clone();
    let f = captured.expect("vector_search must receive a Filter built from categories");
    let s = f.to_string();
    assert!(s.contains("categories"), "filter must reference the categories field: {s}");
    assert!(s.contains("urgent"), "filter must reference the requested category value: {s}");
}

/// When BOTH a string-DSL filter and categories are present, they must be
/// AND-combined — the pushed filter references both predicates.
#[tokio::test]
async fn categories_and_combine_with_string_filter() {
    let (app, storage) = build_app();

    // v0 filter DSL quotes values with SINGLE quotes (`field = 'value'`).
    let body = serde_json::json!({
        "query": "what do we know",
        "k": 5,
        "filter": "source = 'chat'",
        "categories": ["urgent"],
    });
    let resp = app.oneshot(recall_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "filter + categories recall must succeed");

    let captured = storage.captured_filter.lock().clone();
    let f = captured.expect("vector_search must receive a combined Filter");
    let s = f.to_string();
    assert!(s.contains("source"), "combined filter must keep the string-DSL predicate: {s}");
    assert!(s.contains("categories"), "combined filter must add the categories predicate: {s}");
    // An AND-combination surfaces the `And` variant in the serialized Filter.
    assert!(s.contains("And"), "string filter + categories must AND-combine: {s}");
}

/// Too many categories (> 16) must be rejected with 400 `invalid_categories`
/// BEFORE any storage call.
#[tokio::test]
async fn invalid_categories_returns_400() {
    let (app, storage) = build_app();

    let too_many: Vec<String> = (0..17).map(|i| format!("c{i}")).collect();
    let body = serde_json::json!({
        "query": "x",
        "categories": too_many,
    });
    let resp = app.oneshot(recall_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "over-limit categories must be 400");

    let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    assert_eq!(v["error"].as_str().unwrap_or_default(), "invalid_categories");
    assert_eq!(*storage.vector_search_calls.lock(), 0, "a rejected recall must not touch storage");
}
