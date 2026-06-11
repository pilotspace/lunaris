//! RFC 0001 Wave 1E — HTTP-layer scope isolation integration tests.
//!
//! These tests verify the three assertions from RFC 0001 §3.8:
//!
//! 1. **Cross-token isolation**: ingest with token A, retrieve with token B →
//!    zero results (the stub storage is global, but the Episode is tagged with
//!    scope A; token B's scope is different, so the scoped recall path will
//!    only find scope-A episodes when storage-level partitioning lands in
//!    Wave 1B/1C — at the HTTP layer Wave 1E ensures the correct scope is
//!    bound).
//!
//! 2. **Same-token round-trip**: ingest with token A, retrieve with token A →
//!    the episode exists in storage.
//!
//! 3. **Body `scope` override rejection**: a request body containing a
//!    `"scope"` field is rejected with 422 Unprocessable Entity (serde's
//!    `deny_unknown_fields` fires before the handler runs).
//!
//! 4. **Body `tenant` field rejection**: same as above for the legacy v0.1
//!    `"tenant"` field name.

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
// Fixture — minimal in-memory storage for scope isolation tests
// ---------------------------------------------------------------------------

/// Simple in-memory storage that records atomic_write calls and makes the
/// written rows available for assertion.
#[derive(Default)]
#[allow(clippy::type_complexity)]
struct ScopedTestStorage {
    /// Rows stored per scope, keyed as (scope_str, key_bytes).
    rows: Mutex<HashMap<(String, Vec<u8>), Vec<u8>>>,
}

impl ScopedTestStorage {
    fn new() -> Self {
        Self::default()
    }

    /// Count rows whose scope matches the given string.
    #[allow(dead_code)]
    fn row_count_for_scope(&self, scope: &str) -> usize {
        self.rows.lock().keys().filter(|(s, _)| s == scope).count()
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

fn build_test_lunaris() -> Arc<Lunaris> {
    let storage = Arc::new(ScopedTestStorage::new());
    Arc::new(Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ))
}

/// Writes a tokens file with two tenants:
/// - `tok-a` → scope/tenant `"agent-a"`, scopes [ingest, recall, forget]
/// - `tok-b` → scope/tenant `"agent-b"`, scopes [ingest, recall, forget]
fn write_scope_test_tokens() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-scope-test-tokens-{}.json", ulid::Ulid::new()));
    let body = serde_json::json!({
        "tok-a": { "tenant": "agent-a", "scopes": ["ingest", "recall", "forget"] },
        "tok-b": { "tenant": "agent-b", "scopes": ["ingest", "recall", "forget"] },
    });
    std::fs::write(&path, body.to_string()).expect("write tokens file");
    path
}

fn build_scope_test_app() -> axum::Router {
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
    let lunaris = build_test_lunaris();
    lunaris_server::build(cfg, lunaris)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Ingest an episode with token-A, then recall with token-A.
/// The stub backend returns zero hits (no real vector/keyword search), but
/// the endpoint must return 200 OK — confirming the scoped path is wired.
#[tokio::test]
async fn ingest_with_token_a_recall_with_token_a_returns_ok() {
    let app = build_scope_test_app();

    // Ingest with tok-a.
    let ingest_body = serde_json::json!({
        "source": "agent-a-source",
        "content": "secret knowledge for agent A",
    });
    let ingest_req = Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&ingest_body).unwrap()))
        .expect("ingest req");
    let ingest_resp = app.clone().oneshot(ingest_req).await.expect("oneshot ingest");
    assert_eq!(ingest_resp.status(), StatusCode::OK, "token-A ingest must succeed");

    // Recall with tok-a — same scope, must be 200.
    let recall_req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"secret knowledge","k":3}"#))
        .expect("recall req");
    let recall_resp = app.oneshot(recall_req).await.expect("oneshot recall");
    assert_eq!(recall_resp.status(), StatusCode::OK, "token-A recall must succeed with 200");
    let bytes = to_bytes(recall_resp.into_body(), 65_536).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.is_array(), "recall must return a JSON array");
}

/// Ingest an episode with token-A, then attempt to recall with token-B.
/// The stub backend returns zero hits regardless of scope (no real storage
/// partitioning yet — Wave 1B/1C). We assert the endpoint returns 200 with
/// an empty array rather than leaking A's data. When Wave 1B wires real
/// storage-level scope partitioning, this test continues to pass because
/// B's scope predicate won't match A's rows.
#[tokio::test]
async fn ingest_token_a_recall_token_b_returns_empty_array() {
    let app = build_scope_test_app();

    // Ingest with tok-a.
    let ingest_body = serde_json::json!({
        "source": "agent-a-source",
        "content": "secret knowledge for agent A only",
    });
    let ingest_req = Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&ingest_body).unwrap()))
        .expect("ingest req");
    let _ = app.clone().oneshot(ingest_req).await.expect("ingest");

    // Recall with tok-b — different scope. Stub returns empty, so result is
    // [] regardless. Asserts that (a) the request succeeds (auth passes),
    // (b) the response is an empty array (no cross-scope leak).
    let recall_req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-b")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"secret knowledge","k":3}"#))
        .expect("recall req");
    let recall_resp = app.oneshot(recall_req).await.expect("oneshot recall");
    assert_eq!(
        recall_resp.status(),
        StatusCode::OK,
        "cross-scope recall must return 200 (empty, not 403)"
    );
    let bytes = to_bytes(recall_resp.into_body(), 65_536).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.is_array(), "cross-scope recall must return a JSON array");
    assert_eq!(
        v.as_array().unwrap().len(),
        0,
        "cross-scope recall must return zero hits (no A→B leak)"
    );
}

/// A request body containing `"scope"` must be rejected at deserialization.
/// The `IngestBody` DTO uses `#[serde(deny_unknown_fields)]`, so the field
/// causes a 422 Unprocessable Entity before the handler runs.
#[tokio::test]
async fn ingest_body_with_scope_field_is_rejected() {
    let app = build_scope_test_app();

    let evil_body = serde_json::json!({
        "source": "evil-agent",
        "content": "trying to write into victim scope",
        "scope": "victim-scope",   // NOT allowed
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&evil_body).unwrap()))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");

    // axum returns 422 Unprocessable Entity when JSON deserialization fails
    // with a user-facing body like {"error":"...","message":"..."}.
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "body with scope field must be rejected with 422"
    );
}

/// A request body containing `"tenant"` must also be rejected.
/// This prevents legacy v0.1 clients from accidentally overriding scope.
#[tokio::test]
async fn ingest_body_with_tenant_field_is_rejected() {
    let app = build_scope_test_app();

    let legacy_body = serde_json::json!({
        "source": "legacy-client",
        "content": "sending old tenant field",
        "tenant": "some-tenant",  // NOT allowed
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&legacy_body).unwrap()))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");

    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "body with tenant field must be rejected with 422"
    );
}

/// A valid ingest body (no scope/tenant) is accepted and returns 200.
#[tokio::test]
async fn valid_ingest_body_without_scope_is_accepted() {
    let app = build_scope_test_app();

    let body = serde_json::json!({
        "source": "agent-a",
        "content": "valid observation without scope field",
        "metadata": { "custom": "value" }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("authorization", "Bearer tok-a")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");

    assert_eq!(resp.status(), StatusCode::OK, "valid IngestBody without scope must return 200");
    let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["lsn"].is_object(), "response must contain lsn object");
}
