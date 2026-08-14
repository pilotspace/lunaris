//! RED suite for `observability-rollout-maturity` — the /healthz storage probe.
//!
//! The gap (mem0-gap-analysis §3, `routes/healthz.rs:11`): `healthz_handler`
//! ignores `State` and ALWAYS returns 200, so a dead Moon still answers
//! "healthy" → the 5%→100% rollout auto-cutback never trips.
//!
//! Contract (TASK.md §3, FROZEN v1):
//!   GET /healthz → 200 {ok:true,version}  when the storage probe succeeds
//!                → 503 {ok:false,version} when the storage probe fails
//!
//! This file drives the wiring with a `ProbeStorage{healthy}` mock whose
//! `StoragePort::health_check` returns Ok or Err on demand, then asserts the
//! REAL router's `/healthz` status. The `healthy → 200` case also exercises
//! the additive default-Ok path indirectly; `server_smoke::healthz_returns_200`
//! (RecordingStorageWithKeyword, which does NOT override health_check) is the
//! dedicated default-Ok guard.
//!
//! RED reasons, in order: (1) `StoragePort::health_check` does not exist yet →
//! the `impl` below fails to compile; (2) once the trait method lands, the
//! unhealthy case still returns 200 until `healthz_handler` is rewritten.

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
    Embedder, Hlc, HlcClock, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use tower::ServiceExt;

/// Minimal `StoragePort` whose liveness is controllable. Every data method is a
/// trivial no-op — `/healthz` only ever calls `health_check`.
struct ProbeStorage {
    healthy: bool,
}

#[async_trait]
impl StoragePort for ProbeStorage {
    async fn health_check(&self) -> Result<(), StorageError> {
        if self.healthy {
            Ok(())
        } else {
            Err(StorageError::Backend("probe: storage unreachable".into()))
        }
    }

    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        _ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }

    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
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
        _scope: &lunaris_core::Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
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
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for ProbeStorage {
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

fn write_test_tokens_file() -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("lunaris-healthz-tokens-{}.json", ulid::Ulid::new()));
    std::fs::write(&path, serde_json::json!({}).to_string()).expect("write tokens file");
    path
}

/// Build the REAL router with a `Lunaris` backed by `ProbeStorage{healthy}`.
fn build_app(healthy: bool) -> axum::Router {
    let probe = Arc::new(ProbeStorage { healthy });
    let lunaris = Arc::new(Lunaris::with_parts_keyword(
        probe.clone() as Arc<dyn StoragePort>,
        probe as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ));
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file: write_test_tokens_file(),
        rate_per_second: 1000,
        rate_burst: 1000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        http_timeout_secs: 30,
        http_concurrency: 256,
        metrics_disabled: false,
    };
    lunaris_server::build(cfg, lunaris)
}

async fn get_healthz(app: axum::Router) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().uri("/healthz").body(Body::empty()).expect("req");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    (status, json)
}

#[tokio::test]
async fn healthz_healthy_storage_returns_200() {
    let (status, body) = get_healthz(build_app(true)).await;
    assert_eq!(status, StatusCode::OK, "healthy storage must answer 200, got {status}");
    assert_eq!(body["ok"], serde_json::Value::Bool(true), "body must report ok:true");
}

#[tokio::test]
async fn healthz_unhealthy_storage_returns_503() {
    let (status, body) = get_healthz(build_app(false)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a failed storage probe MUST answer 503 so the rollout controller cuts back, got {status}"
    );
    assert_eq!(body["ok"], serde_json::Value::Bool(false), "body must report ok:false");
}
