//! RED suite for `multi-level-memory-categories` — HTTP ingest composition,
//! true-isolation, rejections, and categories persistence.
//!
//! RED reason (runtime, the right reason): `IngestBody` does not yet carry
//! `user_id`/`agent_id`/`session_id`/`categories`, so `deny_unknown_fields`
//! rejects these bodies with 422 instead of composing the scope. After BUILD
//! wires the DTO + handler, the composed write lands at
//! `lunaris:{base}.u-…[.a-…][.s-…]:…` and the base scope sees nothing
//! (true isolation — the freeze's confirmed Mem0 semantic, no roll-up).
//!
//! The storage stub is reused from `scope_routing.rs`; the key addition is
//! that `build_scope_test_app` hands back the `Arc<ScopedTestStorage>` so a
//! test can assert *which* scope a write was tagged with — that is the
//! discriminating proof that the handler composed the scope rather than
//! using the bare JWT base.

#![forbid(unsafe_code)]

use std::collections::HashMap;
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
// Fixture — records atomic_write rows keyed by (scope_str, key_bytes).
// ---------------------------------------------------------------------------

#[derive(Default)]
#[allow(clippy::type_complexity)]
struct ScopedTestStorage {
    rows: Mutex<HashMap<(String, Vec<u8>), Vec<u8>>>,
}

impl ScopedTestStorage {
    fn new() -> Self {
        Self::default()
    }

    fn row_count_for_scope(&self, scope: &str) -> usize {
        self.rows.lock().keys().filter(|(s, _)| s == scope).count()
    }

    fn total_rows(&self) -> usize {
        self.rows.lock().len()
    }

    /// Return the decoded JSON of the first episode value written under
    /// `scope` (episode values are `serde_json::to_vec(&episode)`).
    fn episode_json_for_scope(&self, scope: &str) -> Option<serde_json::Value> {
        let rows = self.rows.lock();
        rows.iter()
            .find(|((s, k), _)| s == scope && String::from_utf8_lossy(k).contains(":episode:"))
            .and_then(|(_, v)| serde_json::from_slice(v).ok())
    }
}

#[async_trait]
impl StoragePort for ScopedTestStorage {
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        let mut rows = self.rows.lock();
        for op in ops {
            if let WriteOp::KvPut { key, value } = op {
                rows.insert((scope.as_str().to_string(), key.clone()), value.clone());
            }
        }
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }

    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
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
impl KeywordPort for ScopedTestStorage {
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

fn write_scope_test_tokens() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-mlvl-tokens-{}.json", ulid::Ulid::new()));
    let body = serde_json::json!({
        "tok-a": { "tenant": "agent-a", "scopes": ["ingest", "recall", "forget"] },
    });
    std::fs::write(&path, body.to_string()).expect("write tokens file");
    path
}

/// Returns the app plus a handle to the underlying storage so tests can
/// assert which scope a write landed in.
fn build_scope_test_app() -> (axum::Router, Arc<ScopedTestStorage>) {
    let storage = Arc::new(ScopedTestStorage::new());
    let lunaris = Arc::new(Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage.clone() as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ));
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file: write_scope_test_tokens(),
        rate_per_second: 1000,
        rate_burst: 1000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        metrics_disabled: false,
    };
    (lunaris_server::build(cfg, lunaris), storage)
}

fn ingest_request(body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("ingest req")
}

async fn error_field(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    v["error"].as_str().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Ingest with user_id + session_id under tenant `agent-a`. The write MUST be
/// tagged with the composed scope `agent-a.u-alice.s-run42`, and the bare base
/// scope `agent-a` MUST see nothing (true isolation — no roll-up).
#[tokio::test]
async fn ingest_composes_scope_and_isolates_base() {
    let (app, storage) = build_scope_test_app();

    let body = serde_json::json!({
        "source": "chat",
        "content": "alice prefers dark roast",
        "user_id": "alice",
        "session_id": "run42",
    });
    let resp = app.oneshot(ingest_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "valid multi-level ingest must succeed");

    assert!(
        storage.row_count_for_scope("agent-a.u-alice.s-run42") >= 1,
        "write must land at the COMPOSED scope, not the bare JWT base"
    );
    assert_eq!(
        storage.row_count_for_scope("agent-a"),
        0,
        "ISOLATION: the base scope must NOT receive the sub-level write (no roll-up)"
    );
}

/// A level id containing the `.` separator (or other non-segment chars) must
/// be rejected with 400 `invalid_level_segment`, and nothing may be written.
#[tokio::test]
async fn invalid_level_segment_returns_400() {
    let (app, storage) = build_scope_test_app();

    let body = serde_json::json!({
        "source": "chat",
        "content": "x",
        "user_id": "al.ice",   // `.` is the composition separator — illegal in a segment
    });
    let resp = app.oneshot(ingest_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "dotted level id must be 400");
    assert_eq!(error_field(resp).await, "invalid_level_segment");
    assert_eq!(storage.total_rows(), 0, "a rejected ingest must write nothing");
}

/// A char-valid id whose composition blows the 128-byte scope cap must be
/// rejected with 400 `scope_too_long` (distinct from `invalid_level_segment`).
#[tokio::test]
async fn scope_too_long_returns_400() {
    let (app, storage) = build_scope_test_app();

    let long_id = "a".repeat(130); // valid chars, but base+".u-"+130 > 128
    let body = serde_json::json!({
        "source": "chat",
        "content": "x",
        "user_id": long_id,
    });
    let resp = app.oneshot(ingest_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "over-long composed scope must be 400");
    assert_eq!(error_field(resp).await, "scope_too_long");
    assert_eq!(storage.total_rows(), 0, "a rejected ingest must write nothing");
}

/// Regression guard on the freeze invariant: a wire `scope` field is still
/// rejected by `deny_unknown_fields` (422) even with the new level fields
/// present — the JWT tenant remains the only partition source of truth.
#[tokio::test]
async fn wire_scope_field_still_rejected() {
    let (app, _storage) = build_scope_test_app();

    let body = serde_json::json!({
        "source": "chat",
        "content": "x",
        "user_id": "alice",
        "scope": "victim-tenant",   // smuggled — must never be honored
    });
    let resp = app.oneshot(ingest_request(&body)).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a wire `scope` field must still be rejected (deny_unknown_fields)"
    );
}

/// Categories supplied on ingest are persisted at `Episode.metadata["categories"]`
/// (episode values are JSON-encoded in the KvPut value).
#[tokio::test]
async fn ingest_persists_categories_in_metadata() {
    let (app, storage) = build_scope_test_app();

    let body = serde_json::json!({
        "source": "chat",
        "content": "x",
        "user_id": "alice",
        "categories": ["urgent", "work"],
    });
    let resp = app.oneshot(ingest_request(&body)).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "valid categories ingest must succeed");

    let ep = storage
        .episode_json_for_scope("agent-a.u-alice")
        .expect("an episode must have been written at the composed scope");
    assert_eq!(
        ep["metadata"]["categories"],
        serde_json::json!(["urgent", "work"]),
        "categories must be stored at Episode.metadata[\"categories\"]"
    );
}
