//! Memory Inspector (Phase 1) — `graph-endpoint` red suite.
//!
//! Executable contract for the SPA graph canvas's traversal route (TASK
//! `graph-endpoint` §3, FROZEN @ v1):
//!
//! - `GET /v1/graph?root=<32-hex>&depth=<n>` — traverse the caller's scope
//!   graph from `root` out to `depth` hops via `StoragePort::graph_traverse`
//!   and return the root-anchored NODE neighborhood
//!   `{ root, depth, nodes:[{id,name,type}], truncated, graph_native:true }`.
//!   Nodes are deduped by id and exclude the root anchor. Edges are out of v1
//!   scope (Moon Legacy dialect can't bind variable-length path edges).
//!
//! ## Why this is red
//!
//! The `/v1/graph` route is not registered and `routes/graph.rs` does not
//! exist, so every request 404s instead of the contracted statuses. The suite
//! compiles against public API plus a local `MockStorage` double whose
//! `graph_traverse` records the `CypherQuery` and returns a canned
//! `GraphResult` — red for the RIGHT reason (missing route/handler).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::{EntityId, Lunaris};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, ScopePage, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use parking_lot::Mutex;
use tower::ServiceExt;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// MockStorage — records graph_traverse calls + returns a canned GraphResult.
// ---------------------------------------------------------------------------

struct MockStorage {
    graph_native: bool,
    graph_fault: bool,
    canned: GraphResult,
    graph_calls: Mutex<Vec<CypherQuery>>,
}

impl MockStorage {
    fn new(graph_native: bool, canned: GraphResult) -> Self {
        Self { graph_native, graph_fault: false, canned, graph_calls: Mutex::new(Vec::new()) }
    }

    fn faulting() -> Self {
        Self {
            graph_native: true,
            graph_fault: true,
            canned: GraphResult::default(),
            graph_calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<CypherQuery> {
        self.graph_calls.lock().clone()
    }
}

#[async_trait]
impl StoragePort for MockStorage {
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
        query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        self.graph_calls.lock().push(query.clone());
        if self.graph_fault {
            return Err(StorageError::Backend("injected graph fault".into()));
        }
        Ok(self.canned.clone())
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
        Err(StorageError::NotSupported("list_scopes not implemented for this backend"))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: self.graph_native,
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
impl KeywordPort for MockStorage {
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

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

fn write_tokens_file(entries: &[(&str, &str, &[&str])]) -> PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-graph-tokens-{}.json", Ulid::new()));
    let mut map = serde_json::Map::new();
    for (tok, tenant, scopes) in entries {
        map.insert(tok.to_string(), serde_json::json!({ "tenant": tenant, "scopes": scopes }));
    }
    std::fs::write(&path, serde_json::to_string(&map).unwrap()).expect("write tokens");
    path
}

fn build_app(storage: Arc<MockStorage>, tokens_file: PathBuf) -> axum::Router {
    let lunaris = Arc::new(Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ));
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file,
        rate_per_second: 10_000,
        rate_burst: 10_000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        metrics_disabled: true,
    };
    lunaris_server::build(cfg, lunaris)
}

async fn get(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(tok) = token {
        builder = builder.header("authorization", format!("Bearer {tok}"));
    }
    let resp = app.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ent(name: &str) -> EntityId {
    EntityId::from_name_and_type(name, "type")
}

/// Build a canned `GraphResult` with the canonical Legacy columns
/// `["id","name","type"]`, the `id` cell = the EntityId's 32-char hex.
fn canned_graph(rows: Vec<(EntityId, &str, &str)>) -> GraphResult {
    GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: rows
            .into_iter()
            .map(|(id, name, typ)| {
                vec![
                    serde_json::Value::String(format!("{id}")),
                    serde_json::Value::String(name.into()),
                    serde_json::Value::String(typ.into()),
                ]
            })
            .collect(),
    }
}

const RECALL: &[&str] = &["recall"];

// ===========================================================================
// MUSTS
// ===========================================================================

/// DISCRIMINATING: proves the handler builds the exact `Graph::anchored`-shaped
/// Legacy CypherQuery against the real `graph_traverse` port (not a stub) and
/// maps the canned node table into the response.
#[tokio::test]
async fn test_graph_neighborhood_and_cypher() {
    let root = ent("Alice");
    let a = ent("Acme");
    let b = ent("Bob");
    let storage = Arc::new(MockStorage::new(
        true,
        canned_graph(vec![(a, "Acme", "org"), (b, "Bob", "person")]),
    ));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}&depth=2"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "graph traversal must be 200; body={body}");
    assert_eq!(body["graph_native"], serde_json::Value::Bool(true));
    assert_eq!(body["depth"].as_u64().unwrap(), 2);
    assert_eq!(body["root"].as_str().unwrap(), root.to_string());

    let nodes = body["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2, "two neighbor nodes");
    assert_eq!(nodes[0]["id"].as_str().unwrap(), a.to_string());
    assert_eq!(nodes[0]["name"].as_str().unwrap(), "Acme");
    assert_eq!(nodes[0]["type"].as_str().unwrap(), "org");

    // The recorded query proves the handler built the right traversal.
    let calls = probe.calls();
    assert_eq!(calls.len(), 1, "exactly one graph_traverse call");
    let q = &calls[0];
    assert_eq!(q.graph, "lunaris_graph", "canonical graph name");
    assert!(q.cypher.contains("[*1..2]"), "depth literal in cypher; got {}", q.cypher);
    assert!(
        q.cypher.contains("WHERE n.id_hex = sid"),
        "anchor MUST filter via WHERE — Moon silently ignores inline-property filters; got {}",
        q.cypher
    );
    assert!(
        !q.cypher.contains("{id_hex: sid}"),
        "must NOT use the inline-property filter form; got {}",
        q.cypher
    );
    let ids = q.params["ids"].as_array().expect("ids param array");
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].as_str().unwrap(), root.to_string(), "root rides in $ids (not the cypher)");
    assert!(q.params.contains_key("k"), "k (LIMIT) param present");
}

#[tokio::test]
async fn test_graph_default_depth() {
    let root = ent("Alice");
    let storage =
        Arc::new(MockStorage::new(true, canned_graph(vec![(ent("Acme"), "Acme", "org")])));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "default-depth traversal must be 200; body={body}");
    assert_eq!(body["depth"].as_u64().unwrap(), 2, "default depth is DEFAULT_GRAPH_HOPS=2");
    assert!(probe.calls()[0].cypher.contains("[*1..2]"), "default depth literal in cypher");
}

#[tokio::test]
async fn test_graph_root_normalized_dedup_excludes_root() {
    let root = ent("Alice");
    let neighbor = ent("Acme");
    // Canned result includes the root itself + a duplicate neighbor.
    let storage = Arc::new(MockStorage::new(
        true,
        canned_graph(vec![
            (root, "Alice", "person"),
            (neighbor, "Acme", "org"),
            (neighbor, "Acme", "org"),
        ]),
    ));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    // Pass the root as UPPERCASE hex — must be normalized to lowercase.
    let upper = root.to_string().to_uppercase();
    let (status, body) = get(&app, &format!("/v1/graph?root={upper}&depth=1"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "uppercase root must normalize; body={body}");
    assert_eq!(body["root"].as_str().unwrap(), root.to_string(), "root echoed lowercase");

    let nodes = body["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1, "root excluded + duplicate collapsed");
    assert_eq!(nodes[0]["id"].as_str().unwrap(), neighbor.to_string());
    for n in nodes {
        assert_ne!(n["id"].as_str().unwrap(), root.to_string(), "anchor never appears in nodes");
    }
}

#[tokio::test]
async fn test_graph_truncated_flag() {
    let root = ent("Alice");
    // DEFAULT_GRAPH_K = 30 rows → the LIMIT was hit.
    let rows: Vec<(EntityId, String, String)> =
        (0..30).map(|i| (ent(&format!("n{i}")), format!("n{i}"), "t".to_string())).collect();
    let canned = GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: rows
            .iter()
            .map(|(id, name, typ)| {
                vec![
                    serde_json::Value::String(format!("{id}")),
                    serde_json::Value::String(name.clone()),
                    serde_json::Value::String(typ.clone()),
                ]
            })
            .collect(),
    };
    let storage = Arc::new(MockStorage::new(true, canned));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}&depth=2"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["truncated"], serde_json::Value::Bool(true), "LIMIT hit → truncated");
}

// ===========================================================================
// REJECTS
// ===========================================================================

#[tokio::test]
async fn test_graph_unavailable_501() {
    let root = ent("Alice");
    // graph_native=false + pipeline disabled (default) → gate trips.
    let storage = Arc::new(MockStorage::new(false, GraphResult::default()));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}&depth=2"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body["error"].as_str().unwrap(), "graph_unavailable");
    assert!(probe.calls().is_empty(), "gated before any traversal");
}

#[tokio::test]
async fn test_graph_invalid_root_400() {
    let storage = Arc::new(MockStorage::new(true, GraphResult::default()));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/graph?root=not-hex&depth=2", Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_root");
    assert!(probe.calls().is_empty(), "rejected before traversal");
}

/// Inspector-UAT change (2026-06-16): an absent/empty `root` is no longer a
/// 400 — it lists ALL nodes in the scope graph (the "explore" entry point,
/// since entity ids are not otherwise browsable). `depth` is irrelevant in this
/// mode; the cypher is a bare `MATCH (n) ... LIMIT $k` — no UNWIND, no
/// variable-length path.
#[tokio::test]
async fn test_graph_empty_root_lists_all_nodes() {
    let a = ent("Acme");
    let b = ent("Bob");
    let storage = Arc::new(MockStorage::new(
        true,
        canned_graph(vec![(a, "Acme", "org"), (b, "Bob", "person")]),
    ));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    // No `root` param at all → all-nodes mode.
    let (status, body) = get(&app, "/v1/graph?depth=2", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "empty root lists all nodes; body={body}");
    assert_eq!(body["graph_native"], serde_json::Value::Bool(true));
    assert!(body["root"].is_null(), "no anchor → root is null; body={body}");
    let nodes = body["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2, "all graph nodes listed");

    let q = &probe.calls()[0];
    assert!(q.cypher.contains("MATCH (n)"), "all-nodes scan; got {}", q.cypher);
    assert!(!q.cypher.contains("UNWIND"), "no anchor UNWIND in all-nodes mode; got {}", q.cypher);
    assert!(!q.cypher.contains("[*1.."), "no var-length path in all-nodes mode; got {}", q.cypher);
    assert!(q.cypher.contains("n.id_hex AS id"), "returns id_hex; got {}", q.cypher);
    assert!(q.params.contains_key("k"), "k (LIMIT) param present");
}

/// `?root=` (present but empty/whitespace) is treated identically to absent.
#[tokio::test]
async fn test_graph_empty_string_root_also_lists_all_nodes() {
    let storage =
        Arc::new(MockStorage::new(true, canned_graph(vec![(ent("Acme"), "Acme", "org")])));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/graph?root=", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "empty-string root → all nodes; body={body}");
    assert!(body["root"].is_null());
    assert_eq!(body["nodes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_graph_zero_depth_400() {
    let root = ent("Alice");
    let storage = Arc::new(MockStorage::new(true, GraphResult::default()));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}&depth=0"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_depth");
    assert!(probe.calls().is_empty(), "rejected before traversal");
}

#[tokio::test]
async fn test_graph_over_cap_depth_400() {
    let root = ent("Alice");
    let storage = Arc::new(MockStorage::new(true, GraphResult::default()));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}&depth=6"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_depth");
    assert!(probe.calls().is_empty(), "depth>MAX_GRAPH_HOPS rejected before traversal");
}

#[tokio::test]
async fn test_graph_storage_error_500() {
    let root = ent("Alice");
    let storage = Arc::new(MockStorage::faulting());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/graph?root={root}&depth=2"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"].as_str().unwrap(), "storage");
    assert!(body["nodes"].is_null(), "no nodes on a backend error");
}

#[tokio::test]
async fn test_graph_missing_token_401() {
    let root = ent("Alice");
    let storage = Arc::new(MockStorage::new(true, GraphResult::default()));
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, _body) = get(&app, &format!("/v1/graph?root={root}&depth=2"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(probe.calls().is_empty(), "handler not reached — no traversal");
}
