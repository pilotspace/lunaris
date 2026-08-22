//! Plan 07-01 — RED integration tests for `POST /v1/recall` `mode=graph`
//! composition.
//!
//! Closes v0.1.0-MILESTONE-AUDIT.md gap:
//! `gaps.integration[lunaris-server::routes::recall → lunaris-retrieve::Graph::anchored]`
//! + `gaps.flows["HTTP recall mode=graph end-to-end"]`.
//!
//! Fixture shape is copied verbatim from `server_smoke.rs::RecordingStorageWithKeyword`
//! with three additions:
//! - `canned_graph: Mutex<GraphResult>` — what `graph_traverse` returns.
//! - `graph_calls: Mutex<Vec<(CypherQuery, Option<Hlc>)>>` — records every
//!   traverse call so tests can prove `Graph::anchored` was composed.
//! - `vector_hits: Mutex<Vec<VectorHit>>` — canned `vector_search` return so
//!   Test 3 (degraded fallback) can observe non-empty hits flow through the
//!   semantic fallback path.
//!
//! Tests:
//! 1. `mode_graph_composes_graph_operator_when_gate_passes` (RED primary)
//! 2. `mode_graph_preserves_501_gate_when_runtime_off_and_graph_native_false`
//! 3. `mode_graph_falls_back_to_semantic_with_degraded_when_no_entities`
//! 4. `mode_semantic_unchanged_by_handler_diff` (regression guard)
//! 5. `mode_graph_parity_across_moon_and_postgres` (`#[ignore]` — live
//!    dual-backend UAT per ROADMAP Phase 7 SC #3b; mirrors Plan 05-04
//!    `helios_scratchpad_smoke.rs` pattern).

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
use lunaris_retrieve::EntityId;
use parking_lot::Mutex;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixture — RecordingStorageWithKeyword (extended from server_smoke.rs with
// graph_native toggle + canned GraphResult + graph_calls recorder + canned
// vector_hits so Test 3 can observe semantic fallback).
// ---------------------------------------------------------------------------

#[allow(clippy::type_complexity)]
#[derive(Default)]
struct RecordingStorageWithKeyword {
    rows: Mutex<HashMap<Vec<u8>, (Vec<u8>, BiTemporal)>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
    published_messages: Mutex<Vec<(String, u16, Bytes)>>,
    /// Plan 07-01 additions — graph-mode composition assertions.
    canned_graph: Mutex<GraphResult>,
    graph_calls: Mutex<Vec<(CypherQuery, Option<Hlc>)>>,
    vector_hits: Mutex<Vec<VectorHit>>,
    /// Toggle for `capabilities().graph_native` — default `true` so Tests 1/3/4
    /// pass the 501 gate. Test 2 flips to `false` via `with_graph_native(false)`.
    graph_native: Mutex<bool>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        let s = Self::default();
        *s.graph_native.lock() = true;
        s
    }

    fn with_graph_native(self, b: bool) -> Self {
        *self.graph_native.lock() = b;
        self
    }

    fn with_canned_graph(self, g: GraphResult) -> Self {
        *self.canned_graph.lock() = g;
        self
    }

    fn with_vector_hits(self, hits: Vec<VectorHit>) -> Self {
        *self.vector_hits.lock() = hits;
        self
    }

    /// Seed a chunk row keyed by `lunaris:chunk:<ulid>` (the format
    /// `lunaris_retrieve::hydrate::chunk_lookup_key` constructs) so the
    /// hydrate pass yields a non-empty Hit list for the Test 3 fallback
    /// assertion. The value is a minimal serialisable `Chunk` — hydrate
    /// drops on parse failure so the shape must match
    /// `lunaris_core::primitives::Chunk` exactly.
    ///
    /// `id` MUST be 16 bytes (ULID bytes). Anything else will fail to
    /// decode in `chunk_lookup_key` and the hit will be silently dropped.
    fn seed_chunk_row(&self, id: &[u8]) {
        assert_eq!(id.len(), 16, "id must be 16 ULID bytes");
        let ulid = ulid::Ulid::from_bytes(id.try_into().expect("16 bytes"));
        let episode_id = ulid::Ulid::new();
        // The HTTP tests use `tok-all` whose JWT carries tenant="t-1", so
        // recall reads under Scope("t-1"). Key + payload scope must match.
        let scope = lunaris_core::Scope::new("t-1").expect("valid scope");
        let payload = serde_json::json!({
            "id": ulid.to_string(),
            // RFC 0001 Wave 0: Chunk now carries a `scope` field.
            "scope": scope.as_str(),
            "episode_id": episode_id.to_string(),
            "text": "stub chunk text",
            "tokens": 3,
            "offset": 0,
            "heading_path": [],
            "overlap_tail": "",
            "bt": {
                "valid": [{"wall_ms": 1, "counter": 0, "node_id": 0}, null],
                "sys":   [{"wall_ms": 1, "counter": 0, "node_id": 0}, null],
            },
        });
        // RFC 0001 Wave 1C / 2.5B: chunk keys are scope-prefixed
        // `lunaris:{scope}:chunk:{ulid}` via lunaris_core::keyspace::chunk_key.
        // The reader hydrate.rs uses the same helper so writer/reader stay in
        // lockstep. The chunk row's scope payload field must match the key's
        // scope segment.
        let key = lunaris_core::keyspace::chunk_key(&scope, ulid);
        let bt = BiTemporal {
            valid: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
            sys: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
        };
        self.rows.lock().insert(key, (serde_json::to_vec(&payload).unwrap(), bt));
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    // RFC 0001 Wave 0: StoragePort methods now take scope: &Scope as first arg.
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    let prior_bt =
                        self.rows.lock().get(key).map(|(_, bt)| *bt).unwrap_or(BiTemporal {
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
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(self.vector_hits.lock().clone())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        // Record the call shape + return the canned GraphResult — this is the
        // primary evidence surface asserted by Test 1.
        self.graph_calls.lock().push((query.clone(), as_of));
        Ok(self.canned_graph.lock().clone())
    }

    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
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
        _scope: &lunaris_core::Scope,
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
        _scope: &lunaris_core::Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.published_messages.lock().push((topic.to_string(), partition, payload));
        Ok(self.published_messages.lock().len() as u64)
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

    async fn queue_depth(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: *self.graph_native.lock(),
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
impl KeywordPort for RecordingStorageWithKeyword {
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
// Helpers — mirror server_smoke.rs shape; parameterised on storage Arc so each
// test can inject its pre-configured fixture variant.
// ---------------------------------------------------------------------------

fn build_test_lunaris_from_storage(storage: Arc<RecordingStorageWithKeyword>) -> Arc<Lunaris> {
    let storage_dyn: Arc<dyn StoragePort> = storage.clone();
    let keyword_dyn: Arc<dyn KeywordPort> = storage;
    Arc::new(Lunaris::with_parts_keyword(
        storage_dyn,
        keyword_dyn,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ))
}

fn write_test_tokens_file() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-server-graph-mode-tokens-{}.json", ulid::Ulid::new()));
    let body = serde_json::json!({
        "tok-all": { "tenant": "t-1", "scopes": ["ingest", "recall", "forget"] },
    });
    std::fs::write(&path, body.to_string()).expect("write tokens file");
    path
}

fn build_test_app_with(lunaris: Arc<Lunaris>) -> axum::Router {
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

/// Build a GraphResult with N `(id, name, type)` rows for testing. Matches the
/// canned_graph_with helper at `crates/lunaris-retrieve/tests/graph_anchored.rs:186-200`
/// verbatim.
fn canned_graph_with(rows: Vec<(EntityId, &str, &str)>) -> GraphResult {
    GraphResult {
        headers: vec!["id".into(), "name".into(), "type".into()],
        rows: rows
            .into_iter()
            .map(|(id, name, typ)| {
                vec![
                    serde_json::Value::String(format!("{}", id)),
                    serde_json::Value::String(name.into()),
                    serde_json::Value::String(typ.into()),
                ]
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mode_graph_composes_graph_operator_when_gate_passes() {
    // Arrange: graph_native=true + canned GraphResult with one row.
    let bob_id = EntityId::from_name_and_type("Bob", "Person");
    let storage = Arc::new(
        RecordingStorageWithKeyword::new()
            .with_graph_native(true)
            .with_canned_graph(canned_graph_with(vec![(bob_id, "Bob", "Person")])),
    );
    let lunaris = build_test_lunaris_from_storage(storage.clone());
    let app = build_test_app_with(lunaris);

    // Act: POST {"query":"what did Alice do at Acme","k":5,"mode":"graph"}.
    // "Alice" and "Acme" are capitalized non-first tokens → extract_query_entities
    // returns two EntityIds → Graph::anchored gets a non-empty Vec →
    // storage.graph_traverse MUST fire.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"what did Alice do at Acme","k":5,"mode":"graph"}"#))
        .expect("req");

    let resp = app.oneshot(req).await.expect("oneshot");

    // Assert 1a: HTTP 200 OK (NOT 501 — gate passed because graph_native=true).
    assert_eq!(resp.status(), StatusCode::OK, "gate should pass when graph_native=true",);

    // Assert 1b: graph_traverse fired TWICE, in order — PRIMARY proof that
    // Graph::anchored was composed into the retrieval root.
    //
    // F16 re-baseline (was: exactly once). The handler now resolves the
    // query's capitalized tokens to real `EntityId`s by asking the graph for
    // them by name, then anchors on what came back. It used to mint
    // `EntityId::from_name_and_type(token, "")` and anchor on that, which
    // matched nothing in any real store — every id is derived from
    // `(name, type)` with a REAL type. Two calls is the fix, not a regression:
    // resolve, then traverse.
    let calls = storage.graph_calls.lock().clone();
    assert_eq!(
        calls.len(),
        2,
        "expected a name-resolution call then an anchored traversal; got {} call(s)",
        calls.len(),
    );

    // Assert 1c: the FIRST call resolves names and does NOT traverse; the
    // SECOND is the anchored traversal, carrying id_hex (W-7 alignment) and
    // [*1..2] (DEFAULT_GRAPH_HOPS=2 per graph.rs:54).
    let resolve = &calls[0].0.cypher;
    assert!(
        resolve.contains("n.name") && !resolve.contains("[*1.."),
        "first call must be the name lookup, not a traversal: {resolve}",
    );
    let cypher = &calls[1].0.cypher;
    assert!(cypher.contains("id_hex"), "W-7 alignment: cypher must use id_hex property: {cypher}",);
    assert!(
        cypher.contains("[*1..2]"),
        "DEFAULT_GRAPH_HOPS=2 must be spliced as [*1..2]: {cypher}",
    );

    // Assert 1d: response body parses as a JSON array (Vec<Hit> shape preserved).
    drop(calls); // release the lock before consuming the response body
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(v.is_array(), "default Accept should yield JSON array (Vec<Hit>)",);
}

#[tokio::test]
async fn mode_graph_preserves_501_gate_when_runtime_off_and_graph_native_false() {
    // Arrange: graph_native=false + graph_pipeline NOT enabled → gate fails.
    let storage = Arc::new(RecordingStorageWithKeyword::new().with_graph_native(false));
    let lunaris = build_test_lunaris_from_storage(storage.clone());
    let app = build_test_app_with(lunaris);

    // Act.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"what did Alice do","mode":"graph"}"#))
        .expect("req");

    let resp = app.oneshot(req).await.expect("oneshot");

    // Assert 2a: HTTP 501 Not Implemented (existing gate preserved — no regression).
    assert_eq!(
        resp.status(),
        StatusCode::NOT_IMPLEMENTED,
        "existing 501 gate must be preserved when runtime off + graph_native=false",
    );

    // Assert 2b: graph_traverse was NOT called (handler short-circuited).
    assert_eq!(
        storage.graph_calls.lock().len(),
        0,
        "handler MUST short-circuit before composing Graph when gate fails",
    );

    // Assert 2c: response body.error == "graph_mode_unavailable".
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        v["error"], "graph_mode_unavailable",
        "501 payload must preserve the graph_mode_unavailable tag verbatim",
    );
}

#[tokio::test]
async fn mode_graph_falls_back_to_semantic_with_degraded_when_no_entities() {
    // Arrange: graph_native=true + canned vector hits so hydrate yields
    // non-empty Hit list. Query has NO capitalized non-first tokens →
    // extract_query_entities returns empty → Graph::anchored empty fast-path
    // → zero graph_traverse calls AND handler falls back to semantic recall
    // marking every returned Hit with degraded=true.
    let v_id_a = vec![1u8; 16];
    let v_id_b = vec![2u8; 16];
    let storage = Arc::new(
        RecordingStorageWithKeyword::new()
            .with_graph_native(true)
            .with_canned_graph(canned_graph_with(vec![]))
            .with_vector_hits(vec![
                VectorHit {
                    id: v_id_a.clone(),
                    score: 0.9,
                    rerank_applied: false,
                    metadata: serde_json::json!({}),
                },
                VectorHit {
                    id: v_id_b.clone(),
                    score: 0.7,
                    rerank_applied: false,
                    metadata: serde_json::json!({}),
                },
            ]),
    );
    // Seed chunk rows keyed by lunaris:chunk:<hex> so hydrate returns
    // non-empty hits. Hydrate parses the payload + fills Hit fields; we
    // intentionally keep the payload stub minimal — hydrate drops on parse
    // failure but the test assertion only needs SOME hits to flow through.
    storage.seed_chunk_row(&v_id_a);
    storage.seed_chunk_row(&v_id_b);

    let lunaris = build_test_lunaris_from_storage(storage.clone());
    let app = build_test_app_with(lunaris);

    // Act.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"show me everything lowercase","mode":"graph"}"#))
        .expect("req");

    let resp = app.oneshot(req).await.expect("oneshot");

    // Assert 3a: HTTP 200 OK (semantic fallback succeeded).
    assert_eq!(resp.status(), StatusCode::OK);

    // Assert 3b: graph_traverse NOT called (empty EntityId list → graph.rs
    // line 204 fast-path OR handler skipped Graph::anchored composition
    // entirely in the no-entities branch).
    assert_eq!(
        storage.graph_calls.lock().len(),
        0,
        "empty entities must NOT trigger graph_traverse",
    );

    // Assert 3c + 3d: response hits non-empty AND every hit carries
    // degraded=true.
    let bytes = to_bytes(resp.into_body(), 65_536).await.expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let hits = v.as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "semantic fallback must yield at least one hit (fixture seeds 2 chunks): {v}",
    );
    for h in hits {
        assert_eq!(
            h["degraded"], true,
            "every hit from graph-mode-with-empty-entities fallback MUST carry degraded=true: {h}",
        );
    }
}

#[tokio::test]
async fn mode_semantic_unchanged_by_handler_diff() {
    // Regression guard — mode=semantic (default) path MUST NOT touch
    // graph_traverse. This catches accidental wiring where the Graph branch
    // somehow composes into every mode.
    let storage = Arc::new(RecordingStorageWithKeyword::new().with_graph_native(true));
    let lunaris = build_test_lunaris_from_storage(storage.clone());
    let app = build_test_app_with(lunaris);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"what did Alice do at Acme","mode":"semantic"}"#))
        .expect("req");

    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        storage.graph_calls.lock().len(),
        0,
        "semantic mode MUST NOT touch graph_traverse — even with capitalized tokens in query",
    );
}

// ---------------------------------------------------------------------------
// DELETED 2026-08-22 (F12): `mode_graph_parity_across_moon_and_postgres`
//
// It demanded `MOON_URL` AND `PG_URL` and diffed the two backends' hit-id sets.
// 0.7.0 deleted `lunaris-storage-postgres`, so the second half of that gate
// could never be satisfied again — the test was `#[ignore]`d with a reason
// saying exactly that, which kept an unrunnable file honest but not useful.
//
// Deleting rather than rewriting Moon-only, because there was nothing to
// rewrite: the test enabled the graph pipeline and immediately recalled
// WITHOUT SEEDING A CORPUS, then asserted the hit sets were non-empty and
// overlapped. Against two live stores it asserted over whatever the operator's
// data happened to contain; the parity claim was the only thing it added, and
// parity has one arm now.
//
// The gap it leaves is real and is NOT covered by the four tests above, which
// all drive `MockGraphStorage`: **there is no live-Moon test that a
// `mode=graph` recall returns hits from a graph corpus this repo seeded.**
// Filed as F16 rather than left implied by a husk — an `#[ignore]`d test reads
// like coverage in a directory listing, and that is how a P0 sat parked for
// four releases (W1.1).
// ---------------------------------------------------------------------------
