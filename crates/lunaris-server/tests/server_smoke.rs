//! Plan 05-01 — server smoke tests.
//!
//! Uses `tower::ServiceExt::oneshot` against an in-process Router built
//! against a stub Lunaris (RecordingStorageWithKeyword fixture, copied from
//! `crates/lunaris/tests/forget_smoke.rs:50-249` per Plan 05-01 Task 4 spec).
//! The full HTTP wire-contract suite lives in Plan 05-03 (protocol
//! conformance — subprocess + reqwest).
//!
//! Tests:
//! 1. `build_router_compiles`
//! 2. `healthz_returns_200`
//! 3. `ingest_requires_bearer_token`
//! 4. `ingest_with_bad_scope_returns_403`
//! 5. `recall_returns_json_default`
//! 6. `recall_sse_streams_when_event_stream_accept`
//! 7. `forget_dry_run_returns_receipt`
//! 8. `forget_hard_without_token_returns_428`

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
    BiTemporal, Embedder, Hlc, HlcClock, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use parking_lot::Mutex;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixture — RecordingStorageWithKeyword (verbatim shape from
// crates/lunaris/tests/forget_smoke.rs lines 56-234).
// Copy pattern, not invention: every method has the actual StoragePort
// signature from crates/lunaris-core/src/storage/port.rs (B-6 from Plan 04-05).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingStorageWithKeyword {
    rows: Mutex<HashMap<Vec<u8>, (Vec<u8>, BiTemporal)>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
    published_messages: Mutex<Vec<(String, u16, Bytes)>>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(&self, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    let prior_bt = self
                        .rows
                        .lock()
                        .get(key)
                        .map(|(_, bt)| *bt)
                        .unwrap_or(BiTemporal {
                            valid: (Hlc::ZERO, None),
                            sys: (Hlc::ZERO, None),
                        });
                    self.rows.lock().insert(key.clone(), (value.clone(), prior_bt));
                }
                WriteOp::KvDelete { key } => {
                    self.rows.lock().remove(key);
                }
                _ => {}
            }
        }
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 1 })
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
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        let snapshot = self.rows.lock().clone();
        let pairs: Vec<Result<(Bytes, Bytes), StorageError>> = snapshot
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, (v, _bt))| Ok((Bytes::from(k), Bytes::from(v))))
            .collect();
        Ok(Box::pin(stream::iter(pairs)))
    }

    async fn read_as_of(
        &self,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        let rows = self.rows.lock();
        Ok(rows.get(key).map(|(v, bt)| Row {
            key: key.to_vec(),
            value: Bytes::from(v.clone()),
            bt: *bt,
        }))
    }

    async fn publish(
        &self,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.published_messages.lock().push((topic.to_string(), partition, payload));
        Ok(self.published_messages.lock().len() as u64)
    }

    async fn subscribe(
        &self,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: false,
            rerank_native: false,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: false,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorageWithKeyword {
    async fn keyword_search(
        &self,
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

/// B-2 fix per Plan 05-01 Task 4: returns a WORKING Arc<Lunaris> backed by
/// RecordingStorageWithKeyword. Construction mirrors
/// `crates/lunaris/tests/forget_smoke.rs::build_lunaris` lines 240-249
/// verbatim — same Lunaris::with_parts_keyword test seam, same StubEmbedder
/// (768), same HlcClock::new(0) (returns Arc<HlcClock> directly per B-6: do
/// NOT double-wrap in Arc::new).
///
/// NO `todo!()` — that would panic at runtime and break
/// `cargo test -p lunaris-server`.
fn build_test_lunaris() -> Arc<Lunaris> {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    Arc::new(Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ))
}

fn write_test_tokens_file() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "lunaris-server-test-tokens-{}.json",
        ulid::Ulid::new()
    ));
    let body = serde_json::json!({
        "tok-ingest": { "tenant": "t-1", "scopes": ["ingest"] },
        "tok-all":    { "tenant": "t-2", "scopes": ["ingest", "recall", "forget"] },
    });
    std::fs::write(&path, body.to_string()).expect("write tokens file");
    path
}

fn build_test_app() -> axum::Router {
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file: write_test_tokens_file(),
        // Smoke tests do not exercise the rate-limit path; keep the burst
        // generous so per-test bursts don't trip 429 noise.
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

#[tokio::test]
async fn build_router_compiles() {
    // No body — just exercising the build path. If this fails to even
    // construct, every other test in this file would also fail.
    let _ = build_test_app();
}

#[tokio::test]
async fn healthz_returns_200() {
    let app = build_test_app();
    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024).await.expect("body bytes");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(v["ok"], true);
    assert!(v["version"].is_string());
}

#[tokio::test]
async fn ingest_requires_bearer_token() {
    let app = build_test_app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/ingest")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"id":"01HX0000000000000000000000","source":"t","content":"hi","bt":{"valid":[{"wall_ms":0,"counter":0,"node_id":0},null],"sys":[{"wall_ms":0,"counter":0,"node_id":0},null]}}"#))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ingest_with_bad_scope_returns_403() {
    let app = build_test_app();
    // tok-all has all 3 scopes; tok-ingest only has `ingest` — reverse and
    // hit /v1/recall with the ingest-only token to assert 403 on missing
    // scope.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-ingest")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"hi","k":1}"#))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(resp.into_body(), 1024).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(v["error"], "forbidden");
}

#[tokio::test]
async fn recall_returns_json_default() {
    let app = build_test_app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"hello","k":3}"#))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    // Empty stub backend returns Vec::new(); body must parse as a JSON array.
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(v.is_array(), "default Accept should yield JSON array (Vec<Hit>)");
}

#[tokio::test]
async fn recall_sse_streams_when_event_stream_accept() {
    let app = build_test_app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"hello","k":3}"#))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/event-stream"),
        "Accept: text/event-stream MUST yield SSE Content-Type, got {ct}"
    );
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    let body = String::from_utf8_lossy(&bytes);
    // Empty result set still emits the terminal `event: done` envelope.
    assert!(
        body.contains("event:done") || body.contains("event: done"),
        "SSE body must contain terminal event:done; body was: {body}"
    );
}

#[tokio::test]
async fn forget_dry_run_returns_receipt() {
    let app = build_test_app();
    let body = serde_json::json!({
        "target": { "Id": ulid::Ulid::new().to_string() },
        "dry_run": true
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/forget")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(v["preview"], true, "dry_run receipt must have preview=true");
}

#[tokio::test]
async fn forget_hard_without_token_returns_428() {
    let app = build_test_app();
    let body = serde_json::json!({
        "target": { "Id": ulid::Ulid::new().to_string() },
        "hard": true
        // confirmation_token deliberately omitted — must trigger 428.
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/forget")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(v["error"], "confirmation_required");
}
