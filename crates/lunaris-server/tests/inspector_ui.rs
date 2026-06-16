//! Memory Inspector (Phase 1) — `inspector-spa` red suite.
//!
//! Executable contract for the served dashboard shell (TASK `inspector-spa`
//! §3, FROZEN @ v1):
//!
//! - `GET /` → 200 `text/html`, the self-contained read-only Inspector shell,
//!   served PUBLICLY at the root (no Bearer required to load the page).
//!
//! These are server-contract assertions over the served body, not browser DOM
//! tests (there is no headless runner in this Rust repo — the freeze [test]
//! flag). They pin the behaviour-level invariants: the shell wires each of the
//! four read endpoints, carries a token field (no hardcoded secret), marks the
//! timeline a disabled Phase-2 affordance, is self-contained (no CDN), renders
//! XSS-safely (`textContent`, never an HTML sink), and is read-only (GET only).
//!
//! ## Why this is red
//!
//! The root `/` route is not registered and `routes/ui.rs` does not exist, so
//! `GET /` 404s. The suite compiles against public API (`lunaris_server::build`
//! + a no-op storage double) — red for the RIGHT reason (missing route/asset).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::Lunaris;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, ScopePage, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use tower::ServiceExt;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// NoopStorage — the UI route touches no storage; this satisfies `build()`.
// ---------------------------------------------------------------------------

struct NoopStorage;

#[async_trait]
impl StoragePort for NoopStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
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
    async fn list_scopes(
        &self,
        _prefix: Option<&str>,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<ScopePage, StorageError> {
        Err(StorageError::NotSupported("noop"))
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
impl KeywordPort for NoopStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

fn write_tokens_file() -> PathBuf {
    let path = std::env::temp_dir().join(format!("lunaris-ui-tokens-{}.json", Ulid::new()));
    std::fs::write(&path, "{}").expect("write tokens");
    path
}

fn build_app() -> axum::Router {
    let storage = Arc::new(NoopStorage);
    let lunaris = Arc::new(Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ));
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file: write_tokens_file(),
        rate_per_second: 10_000,
        rate_burst: 10_000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        metrics_disabled: true,
    };
    lunaris_server::build(cfg, lunaris)
}

/// GET / with NO Authorization header → (status, content-type, body).
async fn fetch_shell() -> (StatusCode, String, String) {
    let app = build_app();
    let resp = app
        .oneshot(Request::builder().method("GET").uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ctype =
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = to_bytes(resp.into_body(), 1 << 22).await.unwrap();
    (status, ctype, String::from_utf8_lossy(&bytes).to_string())
}

// ===========================================================================

#[tokio::test]
async fn test_ui_served_public_html() {
    let (status, ctype, body) = fetch_shell().await;
    assert_eq!(status, StatusCode::OK, "GET / must serve the shell without a token");
    assert!(ctype.contains("text/html"), "content-type must be text/html; got {ctype:?}");
    assert!(!body.is_empty(), "shell body is non-empty");
}

#[tokio::test]
async fn test_ui_wires_all_four_endpoints() {
    let (_, _, body) = fetch_shell().await;
    for path in ["/v1/scopes", "/v1/browse/", "/v1/detail/", "/v1/graph"] {
        assert!(body.contains(path), "shell must wire {path}");
    }
}

#[tokio::test]
async fn test_ui_token_field_no_hardcoded_secret() {
    let (_, _, body) = fetch_shell().await;
    assert!(body.contains(r#"id="token""#), "a token input field is present");
    assert!(body.contains("localStorage"), "the token is persisted/read from localStorage");
    // No embedded JWT (a hardcoded recall token would start with the JWS prefix).
    assert!(!body.contains("Bearer eyJ"), "no hardcoded Bearer JWT in the shell");
}

#[tokio::test]
async fn test_ui_timeline_disabled_phase2() {
    let (_, _, body) = fetch_shell().await;
    assert!(body.contains("Phase 2"), "timeline is labelled a Phase-2 affordance");
    assert!(body.contains("disabled"), "timeline control is disabled");
    assert!(!body.contains("/v1/history"), "no Phase-2 history call is wired");
}

#[tokio::test]
async fn test_ui_self_contained_no_cdn() {
    let (_, _, body) = fetch_shell().await;
    assert!(body.contains("<html"), "the shell is an HTML document"); // anchor: red pre-build
    assert!(!body.contains("http://"), "no external http reference");
    assert!(!body.contains("https://"), "no external https/CDN reference");
}

#[tokio::test]
async fn test_ui_xss_safe_rendering() {
    let (_, _, body) = fetch_shell().await;
    // Build the forbidden DOM-HTML-sink token at runtime so this source file
    // itself stays clean of it.
    let sink = format!("{}{}", "inner", "HTML");
    assert!(body.contains("textContent"), "renders data via textContent");
    assert!(!body.contains(&sink), "never assigns the DOM HTML sink from response data");
}

#[tokio::test]
async fn test_ui_read_only_only_get() {
    let (_, _, body) = fetch_shell().await;
    assert!(body.contains("fetch("), "the shell issues fetch calls"); // anchor: red pre-build
    for verb in ["POST", "PUT", "DELETE", "PATCH"] {
        assert!(!body.contains(verb), "read-only dashboard issues no {verb}");
    }
}
