//! Memory Inspector (Phase 1) — `detail-provenance` red suite.
//!
//! Executable contract for the single-primitive detail route the dashboard's
//! lineage drawer binds to (TASK `detail-provenance` §3, FROZEN @ v1):
//!
//! - `GET /v1/detail/{kind}/{id}` for `kind ∈ episode|chunk|fact|community` —
//!   reads the KV row at `{kind}_key(claims.scope, ulid)` and returns
//!   `{ kind, id, primitive, provenance }`, where `provenance` resolves the
//!   upstream source episode(s) (chunk → `episode_id`, fact →
//!   `source_episode_id`), the fact `confidence`, the fact `entities` (subject
//!   + object EntityIds as 32-char hex), and a `partial` degradation bool.
//! - `kind ∈ entity|relation` → `200 { graph_native: true }` with NO read
//!   (entity/relation detail lives in the graph, served by `graph-endpoint`).
//!
//! ## Why this is red
//!
//! The `/v1/detail/{kind}/{id}` route is not registered and
//! `routes/detail.rs` does not exist, so every request 404s (route miss)
//! instead of the contracted 200/400/404/500/401. The suite compiles against
//! only public API plus a local `MockStorage` test double — red for the RIGHT
//! reason (missing route/handler), not a compile gap.
//!
//! ## Harness
//!
//! Mirrors `browse_endpoints.rs`: `build_app()` → `axum::Router` over a
//! `Lunaris::with_parts_keyword(...)` on an in-memory `MockStorage`; mint a
//! `"recall"` token; drive with `app.oneshot(GET …).bearer(tok)`; assert
//! `(StatusCode, serde_json::Value)`. `MockStorage` adds two detail-specific
//! doubles over the browse harness: a `read_called` probe (asserts the
//! "no read performed" rejections) and a `read_fault_prefix` injector (faults
//! `read_as_of` for keys under a prefix — drives the `partial` degradation and
//! the primitive-read 500).

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
use lunaris_core::keyspace::{community_key, episode_key, fact_key};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, ScopePage, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Community, Embedder, Episode, Hlc, HlcClock, Scope, StorageCapabilities,
    StorageError, StoragePort, StubEmbedder,
};
use parking_lot::Mutex;
use tower::ServiceExt;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// MockStorage — in-memory StoragePort + KeywordPort with a read probe and a
// prefix-scoped read-fault injector.
// ---------------------------------------------------------------------------

/// KV rows keyed by `(scope_str, key_bytes)` → `value_bytes`.
type KvRows = HashMap<(String, Vec<u8>), Vec<u8>>;

struct MockStorage {
    rows: Mutex<KvRows>,
    /// When `Some(p)`, `read_as_of` returns `Err` for any key that starts with
    /// `p` — used to fault the provenance read (partial) or the primitive read
    /// (500) in isolation.
    read_fault_prefix: Option<Vec<u8>>,
    /// Set the first time `read_as_of` is entered — drives the "no read
    /// performed" rejection assertions.
    read_called: AtomicBool,
}

impl MockStorage {
    fn new() -> Self {
        Self {
            rows: Mutex::new(HashMap::new()),
            read_fault_prefix: None,
            read_called: AtomicBool::new(false),
        }
    }

    fn with_read_fault(prefix: Vec<u8>) -> Self {
        Self { read_fault_prefix: Some(prefix), ..Self::new() }
    }

    fn read_called(&self) -> bool {
        self.read_called.load(Ordering::SeqCst)
    }

    fn seed_raw(&self, scope: &Scope, key: Vec<u8>, value: Vec<u8>) {
        self.rows.lock().insert((scope.as_str().to_string(), key), value);
    }

    fn seed_episode(&self, scope: &Scope, e: &Episode) {
        self.seed_raw(scope, episode_key(scope, e.id), serde_json::to_vec(e).unwrap());
    }

    fn seed_community(&self, scope: &Scope, c: &Community) {
        self.seed_raw(scope, community_key(scope, c.id), serde_json::to_vec(c).unwrap());
    }

    /// Seed a `fact:` row in the production at-rest shape — exactly the JSON
    /// `structured_ingest` writes (`subject_id`/`object_id` as raw `[u8;16]`
    /// arrays, `source_episode_id` as a ULID string).
    fn seed_fact_with_source(&self, scope: &Scope, fact_id: Ulid, source_ep: Ulid) {
        let subj = EntityId::from_name_and_type("subj", "type");
        let obj = EntityId::from_name_and_type("obj", "type");
        let v = serde_json::json!({
            "id": fact_id.to_string(),
            "subject_id": subj.0,
            "predicate": "rel",
            "object_id": obj.0,
            "fact_text": "seeded fact",
            "confidence": 0.8_f32,
            "valid_from_iso": "2026-06-16T00:00:00Z",
            "valid_to_iso": serde_json::Value::Null,
            "source_episode_id": source_ep.to_string(),
        });
        self.seed_raw(scope, fact_key(scope, fact_id), serde_json::to_vec(&v).unwrap());
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
        self.read_called.store(true, Ordering::SeqCst);
        if let Some(p) = &self.read_fault_prefix
            && key.starts_with(p)
        {
            return Err(StorageError::Backend("injected read fault".into()));
        }
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
        Err(StorageError::NotSupported("list_scopes not implemented for this backend"))
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
    let path = dir.join(format!("lunaris-detail-tokens-{}.json", Ulid::new()));
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

fn uid(i: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + i, i as u128)
}

fn bt() -> BiTemporal {
    BiTemporal {
        valid: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
        sys: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
    }
}

const RECALL: &[&str] = &["recall"];

/// First `items[].id` of a browse page (used to learn the ULIDs minted by a
/// real ingest without reaching into key layout).
fn first_id(body: &serde_json::Value) -> String {
    body["items"].as_array().expect("items array")[0]["id"].as_str().expect("item id").to_string()
}

// ===========================================================================
// MUSTS
// ===========================================================================

/// DISCRIMINATING (built ≠ wired): a single `ingest_structured_inner` call —
/// the exact fn `ingest_structured` delegates to — writes one episode, its
/// chunk, and one fact (with `source_episode_id`). Learn the ids via the
/// already-built browse endpoint, then prove detail/fact AND detail/chunk
/// resolve provenance back to that one episode. No hand-seeded rows.
#[tokio::test]
async fn test_detail_fact_and_chunk_resolve_via_real_ingest() {
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
        .expect("ingest_structured writes episode + chunk + fact");

    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    // Learn the minted ids from the production browse path.
    let (_, fact_page) = get(&app, "/v1/browse/fact", Some("tok-s")).await;
    let fact_id = first_id(&fact_page);
    let (_, ep_page) = get(&app, "/v1/browse/episode", Some("tok-s")).await;
    let ep_id = first_id(&ep_page);
    let (_, chunk_page) = get(&app, "/v1/browse/chunk", Some("tok-s")).await;
    let chunk_id = first_id(&chunk_page);

    // detail/fact — primitive + resolved provenance.
    let (status, body) = get(&app, &format!("/v1/detail/fact/{fact_id}"), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "detail/fact must be 200; body={body}");
    assert_eq!(body["kind"].as_str().unwrap(), "fact");
    assert_eq!(body["id"].as_str().unwrap(), fact_id, "echoes the path id");
    assert_eq!(body["primitive"]["fact_text"].as_str().unwrap(), "Alice founded Acme");

    let prov = &body["provenance"];
    let conf = prov["confidence"].as_f64().expect("fact confidence is a number");
    assert!((conf - 0.95).abs() < 1e-4, "confidence resolves to the ingested 0.95; got {conf}");
    let srcs = prov["source_episodes"].as_array().expect("source_episodes array");
    assert_eq!(srcs.len(), 1, "the one source episode resolves");
    assert_eq!(srcs[0]["id"].as_str().unwrap(), ep_id, "source episode is the ingested episode");
    let ents = prov["entities"].as_array().expect("entities array");
    assert_eq!(ents.len(), 2, "subject + object entity ids");
    for e in ents {
        let hex = e.as_str().expect("entity id is a hex string");
        assert_eq!(hex.len(), 32, "EntityId renders as 32-char hex; got {hex:?}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "lowercase hex only: {hex:?}");
    }
    assert_eq!(prov["partial"], serde_json::Value::Bool(false), "fully resolved → not partial");

    // detail/chunk — provenance resolves the same source episode via episode_id.
    let (cs, cbody) = get(&app, &format!("/v1/detail/chunk/{chunk_id}"), Some("tok-s")).await;
    assert_eq!(cs, StatusCode::OK, "detail/chunk must be 200; body={cbody}");
    let csrcs = cbody["provenance"]["source_episodes"].as_array().expect("chunk source_episodes");
    assert_eq!(csrcs.len(), 1, "chunk resolves its one source episode");
    assert_eq!(csrcs[0]["id"].as_str().unwrap(), ep_id, "chunk source episode is the ingested one");
}

#[tokio::test]
async fn test_detail_episode_minimal() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    let ep = Episode {
        id: uid(10),
        source: "src".to_string(),
        content: "an observation".to_string(),
        t_ref: None,
        bt: bt(),
        metadata: serde_json::Map::new(),
        scope: s.clone(),
    };
    storage.seed_episode(&s, &ep);
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/episode/{}", uid(10)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "detail/episode must be 200; body={body}");
    assert_eq!(body["primitive"]["content"].as_str().unwrap(), "an observation");
    let prov = &body["provenance"];
    assert_eq!(prov["source_episodes"].as_array().unwrap().len(), 0, "episode IS the source");
    assert_eq!(prov["confidence"], serde_json::Value::Null, "no confidence for an episode");
    assert_eq!(prov["entities"].as_array().unwrap().len(), 0, "no entity refs for an episode");
    assert_eq!(prov["partial"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn test_detail_community_minimal() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
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

    let (status, body) =
        get(&app, &format!("/v1/detail/community/{}", uid(15)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "detail/community must be 200; body={body}");
    assert_eq!(body["primitive"]["summary"].as_str().unwrap(), "a community");
    let prov = &body["provenance"];
    assert_eq!(
        prov["source_episodes"].as_array().unwrap().len(),
        0,
        "v0.3 stores no community provenance"
    );
    assert_eq!(prov["confidence"], serde_json::Value::Null);
    assert_eq!(prov["partial"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn test_detail_fact_dangling_source() {
    let s = Scope::new("agent.s").unwrap();
    let storage = Arc::new(MockStorage::new());
    // Fact points at a source episode that was never written.
    storage.seed_fact_with_source(&s, uid(20), uid(99));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/fact/{}", uid(20)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "a dangling ref still resolves the primitive; body={body}");
    let prov = &body["provenance"];
    assert_eq!(
        prov["source_episodes"].as_array().unwrap().len(),
        0,
        "missing episode → empty list"
    );
    assert_eq!(
        prov["partial"],
        serde_json::Value::Bool(false),
        "a missing ref is normal data, not a fault"
    );
}

#[tokio::test]
async fn test_detail_fact_provenance_read_fault_partial() {
    let s = Scope::new("agent.s").unwrap();
    // Fault ONLY the source-episode read; the fact primitive read still succeeds.
    let storage = Arc::new(MockStorage::with_read_fault(episode_key(&s, uid(30))));
    storage.seed_fact_with_source(&s, uid(21), uid(30));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/fact/{}", uid(21)), Some("tok-s")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a provenance-read error NEVER cascades into a 500; body={body}"
    );
    assert!(body["primitive"].is_object(), "the primitive still resolves");
    let prov = &body["provenance"];
    assert_eq!(
        prov["source_episodes"].as_array().unwrap().len(),
        0,
        "the faulted episode is omitted"
    );
    assert_eq!(prov["partial"], serde_json::Value::Bool(true), "degraded → partial flag set");
}

#[tokio::test]
async fn test_detail_entity_graph_native() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/entity/{}", uid(1)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "entity detail is 200 graph-native; body={body}");
    assert_eq!(body["graph_native"], serde_json::Value::Bool(true), "graph_native flag set");
    assert!(!probe.read_called(), "graph-native kinds perform no storage read");
}

#[tokio::test]
async fn test_detail_relation_graph_native() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/relation/{}", uid(1)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::OK, "relation detail is 200 graph-native; body={body}");
    assert_eq!(body["graph_native"], serde_json::Value::Bool(true), "graph_native flag set");
    assert!(!probe.read_called(), "graph-native kinds perform no storage read");
}

#[tokio::test]
async fn test_detail_cross_scope_404() {
    let other = Scope::new("agent.other").unwrap();
    let storage = Arc::new(MockStorage::new());
    // Fact lives ONLY in scope OTHER.
    storage.seed_fact_with_source(&other, uid(40), uid(41));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    // Scope-S token requests OTHER's fact id → keyed by claims.scope=S → miss.
    let (status, body) = get(&app, &format!("/v1/detail/fact/{}", uid(40)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-scope id is invisible; body={body}");
    assert_eq!(body["error"].as_str().unwrap(), "not_found");
    assert!(body["primitive"].is_null(), "no OTHER-scope primitive leaks");
}

// ===========================================================================
// REJECTS
// ===========================================================================

#[tokio::test]
async fn test_detail_invalid_kind_400() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/widgets/{}", uid(1)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_kind");
    assert!(!probe.read_called(), "unknown kind rejected pre-read");
}

#[tokio::test]
async fn test_detail_invalid_id_400() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, "/v1/detail/fact/not-a-ulid", Some("tok-s")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"].as_str().unwrap(), "invalid_id");
    assert!(!probe.read_called(), "malformed id rejected pre-read");
}

#[tokio::test]
async fn test_detail_not_found_404() {
    let storage = Arc::new(MockStorage::new());
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/fact/{}", uid(404)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"].as_str().unwrap(), "not_found");
    assert!(body["primitive"].is_null(), "no primitive on a miss");
}

#[tokio::test]
async fn test_detail_primitive_storage_error_500() {
    let s = Scope::new("agent.s").unwrap();
    // Fault the fact-key (primitive) read itself.
    let storage = Arc::new(MockStorage::with_read_fault(fact_key(&s, uid(50))));
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, body) = get(&app, &format!("/v1/detail/fact/{}", uid(50)), Some("tok-s")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"].as_str().unwrap(), "storage");
    assert!(body["primitive"].is_null(), "no primitive on a backend error");
}

#[tokio::test]
async fn test_detail_missing_token_401() {
    let storage = Arc::new(MockStorage::new());
    let probe = storage.clone();
    let app = build_app(storage, write_tokens_file(&[("tok-s", "agent.s", RECALL)]));

    let (status, _body) = get(&app, &format!("/v1/detail/fact/{}", uid(1)), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!probe.read_called(), "handler not reached — no read performed");
}
