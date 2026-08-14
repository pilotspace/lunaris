//! Memory Inspector (Phase 1) — `browse-endpoints` red suite.
//!
//! Executable contract for the two query-less browse routes the dashboard binds
//! to (TASK `browse-endpoints` §3, FROZEN @ v1):
//!
//! - `GET /v1/scopes?prefix=&cursor=&limit=` — CROSS-SCOPE partition enumeration
//!   over `Lunaris::list_scopes` (deliberately NOT filtered by `claims.scope`).
//! - `GET /v1/browse/{kind}?cursor=&limit=` — paginate one scope's primitives of
//!   `kind ∈ episode|chunk|entity|relation|fact|community`, scoped to the JWT
//!   `claims.scope` ONLY, dispatching `{kind}_prefix → scan_page::<T> → Page<T>`.
//!
//! ## Why this is red
//!
//! Neither route is registered yet and `routes/browse.rs` does not exist, so
//! every request 404s instead of the contracted 200/400/500/501/401. The suite
//! compiles against only public API (`lunaris_server::build`, `Lunaris`,
//! keyspace + primitive re-exports) plus a local `MockStorage` test double — it
//! is red for the RIGHT reason (missing routes/handler), not a compile gap.
//!
//! ## Harness
//!
//! Mirrors `multi_agent_uat.rs`: `build_app()` → `axum::Router` over a
//! `Lunaris::with_parts_keyword(...)` on an in-memory `MockStorage`; mint a
//! `"recall"` token; drive with `app.oneshot(GET …).bearer(tok)`; assert
//! `(StatusCode, serde_json::Value)`. `MockStorage` is configurable for the two
//! error doubles: `list_scopes → NotSupported` (501) and a fault-injecting
//! `scan_range` (storage 500), plus a `scan_called` probe for the
//! "no scan performed" rejections.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::{self, BoxStream};
use lunaris::structured_ingest::ingest_structured_inner;
use lunaris::{EntityId, EpisodeBuilder, FactInput, Lunaris, StructuredIngest};
use lunaris_core::keyspace::{chunk_key, community_key, episode_key, fact_key};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, ScopePage, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Chunk, Community, Embedder, Episode, Hlc, HlcClock, Scope, StorageCapabilities,
    StorageError, StoragePort, StubEmbedder,
};
use lunaris_extract::types::Fact as ExtractFact;
use parking_lot::Mutex;
use tower::ServiceExt;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// MockStorage — in-memory StoragePort + KeywordPort, configurable for the
// two error doubles the §4 plan requires.
// ---------------------------------------------------------------------------

enum ScopesMode {
    /// Trait-default behavior: `list_scopes` → `NotSupported` (Postgres-like).
    NotSupported,
    /// `list_scopes` → `Ok(ScopePage { scopes, next_cursor: None })`.
    Enumerate(Vec<Scope>),
}

/// KV rows keyed by `(scope_str, key_bytes)` → `value_bytes`.
type KvRows = HashMap<(String, Vec<u8>), Vec<u8>>;

struct MockStorage {
    /// KV rows: (scope_str, key_bytes) → value_bytes.
    rows: Mutex<KvRows>,
    /// How `list_scopes` behaves (501 vs enumeration).
    scopes_mode: ScopesMode,
    /// When true, `scan_range` yields a single mid-stream `Err` (storage 500).
    scan_fault: bool,
    /// Set the first time `scan_range` is entered — drives the
    /// "no scan performed" rejection assertions.
    scan_called: AtomicBool,
}

impl MockStorage {
    fn new() -> Self {
        Self {
            rows: Mutex::new(HashMap::new()),
            scopes_mode: ScopesMode::NotSupported,
            scan_fault: false,
            scan_called: AtomicBool::new(false),
        }
    }

    fn with_scopes(scopes: Vec<Scope>) -> Self {
        Self { scopes_mode: ScopesMode::Enumerate(scopes), ..Self::new() }
    }

    fn with_scan_fault() -> Self {
        Self { scan_fault: true, ..Self::new() }
    }

    fn scan_called(&self) -> bool {
        self.scan_called.load(Ordering::SeqCst)
    }

    fn seed_raw(&self, scope: &Scope, key: Vec<u8>, value: Vec<u8>) {
        self.rows.lock().insert((scope.as_str().to_string(), key), value);
    }

    /// Seed a `fact:` row in the REAL at-rest shape (`lunaris_extract::Fact`).
    fn seed_extract_fact(&self, scope: &Scope, f: &ExtractFact) {
        self.seed_raw(scope, fact_key(scope, f.id), serde_json::to_vec(f).unwrap());
    }

    fn seed_episode(&self, scope: &Scope, e: &Episode) {
        self.seed_raw(scope, episode_key(scope, e.id), serde_json::to_vec(e).unwrap());
    }

    fn seed_chunk(&self, scope: &Scope, c: &Chunk) {
        self.seed_raw(scope, chunk_key(scope, c.id), serde_json::to_vec(c).unwrap());
    }

    fn seed_community(&self, scope: &Scope, c: &Community) {
        self.seed_raw(scope, community_key(scope, c.id), serde_json::to_vec(c).unwrap());
    }
}

#[async_trait]
impl StoragePort for MockStorage {
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        let mut rows = self.rows.lock();
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    rows.insert((scope.as_str().to_string(), key.clone()), value.clone());
                }
                WriteOp::KvDelete { key } => {
                    rows.remove(&(scope.as_str().to_string(), key.clone()));
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
        scope: &Scope,
        prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        self.scan_called.store(true, Ordering::SeqCst);
        if self.scan_fault {
            let item: Result<(Bytes, Bytes), StorageError> =
                Err(StorageError::Backend("injected scan fault".into()));
            return Ok(Box::pin(stream::iter(vec![item])));
        }
        let scope_str = scope.as_str().to_string();
        let snapshot = self.rows.lock().clone();
        let pairs: Vec<Result<(Bytes, Bytes), StorageError>> = snapshot
            .into_iter()
            .filter(|((s, k), _)| s == &scope_str && k.starts_with(prefix))
            .map(|((_, k), v)| Ok((Bytes::from(k), Bytes::from(v))))
            .collect();
        Ok(Box::pin(stream::iter(pairs)))
    }

    async fn read_as_of(
        &self,
        scope: &Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        let rows = self.rows.lock();
        let lookup = (scope.as_str().to_string(), key.to_vec());
        Ok(rows.get(&lookup).map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v.clone()),
            bt: BiTemporal {
                valid: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
                sys: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
            },
        }))
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
        match &self.scopes_mode {
            ScopesMode::Enumerate(scopes) => {
                Ok(ScopePage { scopes: scopes.clone(), next_cursor: None })
            }
            ScopesMode::NotSupported => {
                Err(StorageError::NotSupported("list_scopes not implemented for this backend"))
            }
        }
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
    let path = dir.join(format!("lunaris-browse-tokens-{}.json", Ulid::new()));
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
        http_timeout_secs: 30,
        http_concurrency: 256,
        metrics_disabled: true,
    };
    lunaris_server::build(cfg, lunaris)
}

/// GET `uri` (optionally bearer-authed) → (status, parsed-or-Null body).
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
// Fixtures — explicit, monotonically increasing ULIDs so key order == ULID
// order and the pagination scenarios are deterministic.
// ---------------------------------------------------------------------------

fn uid(i: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + i, i as u128)
}

fn bt() -> BiTemporal {
    BiTemporal {
        valid: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
        sys: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
    }
}

/// A `fact:` row in the production at-rest shape (`lunaris_extract::Fact`) —
/// NOT `core::Fact`. No `scope`/`bt`/`provenance` fields; scope partitioning is
/// the KV key prefix, not a value field.
fn extract_fact(id: Ulid, text: &str) -> ExtractFact {
    ExtractFact {
        id,
        subject_id: EntityId::from_name_and_type("subj", "type"),
        predicate: "rel".to_string(),
        object_id: EntityId::from_name_and_type("obj", "type"),
        fact_text: text.to_string(),
        confidence: 0.9,
        valid_from_iso: "2026-06-16T00:00:00Z".to_string(),
        valid_to_iso: None,
    }
}

const RECALL: &[&str] = &["recall"];

// ===========================================================================
// MUSTS
// ===========================================================================

#[tokio::test]
async fn test_browse_fact_scoped_ordered_page() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    for i in 1..=3u64 {
        storage.seed_extract_fact(&s, &extract_fact(uid(i), &format!("fact {i}")));
    }
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact?limit=2", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "browse fact must be 200; body={body}");

    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "limit=2 yields exactly 2 items");
    assert_eq!(items[0]["id"].as_str().unwrap(), uid(1).to_string(), "ULID-ascending: u1 first");
    assert_eq!(items[1]["id"].as_str().unwrap(), uid(2).to_string(), "ULID-ascending: u2 second");
    for it in items {
        // extract::Fact carries no `scope` field — scoping is the KV key prefix,
        // which the mock's scan_range enforces. Assert the full primitive shape.
        assert!(it["fact_text"].is_string(), "full primitive JSON (fact_text present)");
        assert!(it["confidence"].is_number(), "confidence present");
    }
    assert!(body["next_cursor"].is_string(), "more remain → next_cursor present");
    assert!(body["graph_native"].is_null(), "KV kind carries no graph_native flag");
}

#[tokio::test]
async fn test_browse_pagination_walks_once() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    for i in 1..=3u64 {
        storage.seed_extract_fact(&s, &extract_fact(uid(i), &format!("fact {i}")));
    }
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (s1, p1) = get(&app, "/v1/browse/fact?limit=2", Some("tok-s")).await;
    assert_eq!(s1, StatusCode::OK);
    let page1: Vec<String> = p1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(page1, vec![uid(1).to_string(), uid(2).to_string()]);
    let cursor = p1["next_cursor"].as_str().expect("page 1 has a cursor").to_string();

    let (s2, p2) =
        get(&app, &format!("/v1/browse/fact?limit=2&cursor={cursor}"), Some("tok-s")).await;
    assert_eq!(s2, StatusCode::OK);
    let page2: Vec<String> = p2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(page2, vec![uid(3).to_string()], "page 2 is the remaining item");
    assert!(p2["next_cursor"].is_null(), "last page → next_cursor null");

    // No skip / no repeat across the boundary.
    let mut all = page1.clone();
    all.extend(page2.clone());
    assert_eq!(all, vec![uid(1).to_string(), uid(2).to_string(), uid(3).to_string()]);
}

#[tokio::test]
async fn test_each_kv_kind_browsable() {
    // KV-backed kinds only: episode, chunk, community (core primitives) + fact
    // (extract::Fact). entity/relation are graph-native — covered separately.
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());

    let ep = Episode {
        id: uid(10),
        source: "src".to_string(),
        content: "content".to_string(),
        t_ref: None,
        bt: bt(),
        metadata: serde_json::Map::new(),
        scope: s.clone(),
    };
    storage.seed_episode(&s, &ep);

    let ch = Chunk {
        id: uid(11),
        scope: s.clone(),
        episode_id: uid(10),
        text: "chunk text".to_string(),
        tokens: 2,
        offset: 0,
        heading_path: vec![],
        overlap_tail: String::new(),
        embedding: None,
        bt: bt(),
        parent_id: None,
    };
    storage.seed_chunk(&s, &ch);

    storage.seed_extract_fact(&s, &extract_fact(uid(14), "a fact"));

    let com = Community {
        id: uid(15),
        scope: s.clone(),
        level: 0,
        parent: None,
        members: vec![],
        summary: "a community".to_string(),
        summary_embedding: None,
        bt: bt(),
    };
    storage.seed_community(&s, &com);

    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let expect =
        [("episode", uid(10)), ("chunk", uid(11)), ("fact", uid(14)), ("community", uid(15))];
    for (kind, id) in expect {
        let (status, body) = get(&app, &format!("/v1/browse/{kind}"), Some("tok-s")).await;
        assert_eq!(status, StatusCode::OK, "{kind} must be 200; body={body}");
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1, "{kind} page holds its one seeded item");
        assert_eq!(items[0]["id"].as_str().unwrap(), id.to_string(), "{kind} item id");
        assert!(body["graph_native"].is_null(), "{kind} is KV-backed — no graph_native flag");
    }
}

#[tokio::test]
async fn test_scopes_enumeration() {
    let a = Scope::new("agent.a").unwrap();
    let b = Scope::new("agent.b").unwrap();
    let storage = Arc::new(MockStorage::with_scopes(vec![a.clone(), b.clone()]));
    // Token bound to agent.a — enumeration must still surface agent.b (cross-scope).
    let app = build_app(storage, write_tokens_file(&[("tok-a", "agent.a", RECALL)]));

    let (status, body) = get(&app, "/v1/scopes", Some("tok-a")).await;
    assert_eq!(status, StatusCode::OK, "scopes enumeration must be 200; body={body}");
    let scopes: Vec<String> = body["scopes"]
        .as_array()
        .expect("scopes array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(scopes.contains(&"agent.a".to_string()), "lists own scope");
    assert!(scopes.contains(&"agent.b".to_string()), "lists OTHER scope (not filtered)");
    assert!(body["next_cursor"].is_null(), "single page → next_cursor null");
}

#[tokio::test]
async fn test_browse_ignores_wire_scope() {
    let s = Scope::new("agent.s").unwrap();
    let other = Scope::new("agent.other").unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.seed_extract_fact(&s, &extract_fact(uid(1), "scope S fact"));
    storage.seed_extract_fact(&other, &extract_fact(uid(2), "scope OTHER fact"));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    // Smuggled ?scope=agent.other must be IGNORED (claims.scope wins).
    let (status, body) = get(&app, "/v1/browse/fact?scope=agent.other", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "wire scope ignored, still 200; body={body}");
    let items = body["items"].as_array().expect("items array");
    // extract::Fact has no scope field — assert isolation by id: only S's fact
    // (uid 1) is returned; the OTHER-scope fact (uid 2) never leaks.
    assert_eq!(items.len(), 1, "only the single scope-S fact");
    assert_eq!(items[0]["id"].as_str().unwrap(), uid(1).to_string(), "the scope-S fact");
    for it in items {
        assert_ne!(it["id"].as_str().unwrap(), uid(2).to_string(), "OTHER fact never leaks");
    }
}

/// DISCRIMINATING (built ≠ wired): seed a `fact:` row through the REAL
/// production write path (`ingest_structured_inner`, the exact fn the handle's
/// `ingest_structured` delegates to) — NOT a hand-seeded row — then prove
/// browse/fact reads it. With the old `core::Fact` dispatch this 500s
/// (corrupt_row); only the `extract::Fact` dispatch returns 200.
#[tokio::test]
async fn test_browse_fact_via_real_ingest_structured() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);
    let valid_from: DateTime<Utc> = "2026-06-16T00:00:00Z".parse().unwrap();

    let payload =
        StructuredIngest::new(EpisodeBuilder::new("agent", "Alice founded Acme in 2020."))
            .with_facts(vec![FactInput {
                fact_text: "Alice founded Acme".to_string(),
                subject_name: "Alice".to_string(),
                subject_type: "person".to_string(),
                predicate: "founded".to_string(),
                object_name: "Acme".to_string(),
                object_type: "org".to_string(),
                confidence: 0.95,
                valid_from,
                valid_to: None,
            }]);
    ingest_structured_inner(storage.as_ref(), &embedder, clock.as_ref(), payload, s.clone())
        .await
        .expect("ingest_structured writes the production fact row");

    let app = build_app(storage.clone(), write_tokens_file(&[("tok-s", "agent.s", RECALL)]));
    let (status, body) = get(&app, "/v1/browse/fact", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "browse/fact must read the real ingest row; body={body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "exactly the one ingested fact");
    assert_eq!(items[0]["fact_text"].as_str().unwrap(), "Alice founded Acme");
    assert!(items[0]["confidence"].is_number(), "confidence present in the stored fact");
}

/// The extractor-path shape (`extract::Fact` WITHOUT `source_episode_id`) also
/// deserializes — proving both ingest shapes round-trip through browse/fact.
#[tokio::test]
async fn test_browse_fact_extractor_shape() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.seed_extract_fact(&s, &extract_fact(uid(1), "extractor-path fact"));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "extractor-shape fact must be 200; body={body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["fact_text"].as_str().unwrap(), "extractor-path fact");
}

#[tokio::test]
async fn test_browse_entity_graph_native() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/entity", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "entity browse is 200 graph-native; body={body}");
    assert_eq!(body["graph_native"], serde_json::Value::Bool(true), "graph_native flag set");
    assert_eq!(body["items"].as_array().unwrap().len(), 0, "no KV items fabricated");
    assert!(body["next_cursor"].is_null(), "graph-native page has null cursor");
    assert!(!probe.scan_called(), "graph-native kinds perform no scan");
}

#[tokio::test]
async fn test_browse_relation_graph_native() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/relation", Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "relation browse is 200 graph-native; body={body}");
    assert_eq!(body["graph_native"], serde_json::Value::Bool(true), "graph_native flag set");
    assert_eq!(body["items"].as_array().unwrap().len(), 0, "no KV items fabricated");
    assert!(body["next_cursor"].is_null());
    assert!(!probe.scan_called(), "graph-native kinds perform no scan");
}

// ===========================================================================
// REJECTS
// ===========================================================================

#[tokio::test]
async fn test_invalid_kind_400() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/widgets", Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_kind");
    assert!(!probe.scan_called(), "unknown kind rejected pre-scan — no scan performed");
}

#[tokio::test]
async fn test_zero_limit_400() {
    let storage = Arc::new(MockStorage::new());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact?limit=0", Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_limit");
    assert!(body["items"].is_null(), "no items on a rejected request");
}

#[tokio::test]
async fn test_over_cap_limit_400() {
    let storage = Arc::new(MockStorage::new());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact?limit=501", Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "limit_too_large");
    assert!(body["items"].is_null(), "no items on a rejected request");
}

#[tokio::test]
async fn test_malformed_cursor_400() {
    let storage = Arc::new(MockStorage::new());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact?cursor=not-a-ulid", Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_cursor");
    assert!(body["items"].is_null(), "no items on a rejected request");
}

#[tokio::test]
async fn test_corrupt_row_500() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    // A fact-prefixed key (valid ULID suffix) whose VALUE is not a Fact.
    storage.seed_raw(&s, fact_key(&s, uid(1)), br#"{"not":"a fact"}"#.to_vec());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact", Some("tok-s")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"].as_str().unwrap(), "corrupt_row");
    assert!(body["items"].is_null(), "no partial page returned as if complete");
}

#[tokio::test]
async fn test_scopes_not_supported_501() {
    // Default mock: list_scopes → NotSupported (Postgres-like).
    let storage = Arc::new(MockStorage::new());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/scopes", Some("tok-s")).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body["error"].as_str().unwrap(), "not_supported");
    assert!(body["scopes"].is_null(), "no scope list fabricated");
}

#[tokio::test]
async fn test_storage_error_500() {
    let storage = Arc::new(MockStorage::with_scan_fault());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/browse/fact", Some("tok-s")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"].as_str().unwrap(), "storage");
    assert!(body["items"].is_null(), "no partial page on a mid-stream error");
}

#[tokio::test]
async fn test_missing_token_401() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    // No Authorization header → the existing scoped_auth("recall") layer rejects.
    let (status, _body) = get(&app, "/v1/browse/fact", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!probe.scan_called(), "handler not reached — no scan performed");
}
