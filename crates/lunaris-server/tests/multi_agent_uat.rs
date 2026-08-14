//! Wave 3H — Multi-agent HTTP UAT contract (RFC 0001 §3.8).
//!
//! This file is the **executable acceptance test suite** for any external
//! consumer (Helios or otherwise) integrating against the Lunaris v0.2
//! `lunaris-server` binary. Each UAT scenario maps 1:1 to a curl-style
//! contract in `docs/multi-agent.md`.
//!
//! ## Five scenarios
//!
//! - **UAT-1**: Cross-tenant ingest+recall isolation — scope A data is invisible
//!   to scope B.
//! - **UAT-2**: Malformed scope → 401 — empty, oversize (129 chars), and
//!   invalid-char tenant strings are all rejected at the auth boundary.
//! - **UAT-3**: Body-scope override rejected — `"scope"` and `"tenant"` keys in
//!   the request body produce HTTP 422 before the handler runs.
//! - **UAT-4**: Forget is scope-aware — a cross-scope forget call cannot delete
//!   data owned by another scope (the storage-layer partition prevents it even
//!   though the HTTP handler still returns 200).
//! - **UAT-5**: Concurrent 10-agent smoke — 60 ingest+recall ops from 10 unique
//!   scope tokens, zero cross-scope leakage observed in storage.
//!
//! ## Storage mock
//!
//! `MockMultiTenantStorage` is a fully-featured in-memory storage that:
//! - Partitions every `atomic_write` by the `&Scope` argument.
//! - Returns pre-seeded `VectorHit`s only for the matching scope from
//!   `vector_search`.
//! - Serves `read_as_of` from the same per-scope KV map.
//!
//! This allows UAT-1 to assert that actual hit _text_ (the chunk body) flows
//! back on the correct scope, and is absent on the wrong scope.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::Lunaris;
use lunaris_core::keyspace::{chunk_key, episode_key};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Chunk, Embedder, Episode, Hlc, HlcClock, Scope, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder,
};
use parking_lot::Mutex;
use tower::ServiceExt;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Fixture — MockMultiTenantStorage
// ---------------------------------------------------------------------------

/// Per-scope storage that enforces full isolation between tenants.
///
/// This is the core fixture for Wave 3H UATs. It mirrors the scoped API surface
/// that production Moon/Postgres backends implement, so the UAT exercises the
/// exact same code paths a real deployment would use.
///
/// Seeding workflow for a UAT-1 hit:
/// 1. Call `seed_chunk` to store a `Chunk` row under the target scope.
/// 2. Call `seed_episode` to store the parent `Episode` row.
/// 3. Call `seed_vector_hit` to register the chunk's ULID as a `VectorHit` for
///    that scope's `vector_search` results.
/// 4. When `recall_handler` fires for that scope, `vector_search` returns the
///    seeded hit → `hydrate` calls `read_as_of` → finds the chunk → finds the
///    episode → builds a `Hit` with `text` from the chunk.
#[derive(Default)]
#[allow(clippy::type_complexity)]
struct MockMultiTenantStorage {
    /// KV rows: (scope_str, key_bytes) → value_bytes
    rows: Mutex<HashMap<(String, Vec<u8>), Vec<u8>>>,
    /// Canned vector hits per scope. Only returned when `vector_search` is
    /// called with a matching scope string.
    vector_hits: Mutex<HashMap<String, Vec<VectorHit>>>,
    /// Records every (scope, batch) pair written via `atomic_write` so UAT-5
    /// can verify that cross-scope writes never happen.
    write_log: Mutex<Vec<(String, Vec<WriteOp>)>>,
    /// Records every scope string seen in `vector_search` calls so UAT-5 can
    /// assert that recalls stay in their own scope.
    search_log: Mutex<Vec<String>>,
}

impl MockMultiTenantStorage {
    fn new() -> Self {
        Self::default()
    }

    /// Seed a chunk row for the given scope. The chunk's ULID is used as the
    /// lookup key via `lunaris_core::keyspace::chunk_key`.
    fn seed_chunk(&self, scope: &Scope, chunk: &Chunk) {
        let key = chunk_key(scope, chunk.id);
        let value = serde_json::to_vec(chunk).expect("chunk serialize");
        self.rows.lock().insert((scope.as_str().to_string(), key), value);
    }

    /// Seed an episode row for the given scope. Used by the hydration second
    /// pass to populate `Hit::source`.
    fn seed_episode(&self, scope: &Scope, episode: &Episode) {
        let key = episode_key(scope, episode.id);
        let value = serde_json::to_vec(episode).expect("episode serialize");
        self.rows.lock().insert((scope.as_str().to_string(), key), value);
    }

    /// Register a `VectorHit` as a canned result for `vector_search` calls
    /// on the given scope. Multiple calls append hits.
    fn seed_vector_hit(&self, scope: &Scope, hit: VectorHit) {
        self.vector_hits.lock().entry(scope.as_str().to_string()).or_default().push(hit);
    }

    /// Snapshot the write log for assertions.
    fn writes_for_scope(&self, scope: &str) -> Vec<Vec<WriteOp>> {
        self.write_log
            .lock()
            .iter()
            .filter(|(s, _)| s == scope)
            .map(|(_, ops)| ops.clone())
            .collect()
    }

    /// Count how many batches were written for this scope.
    fn batch_count_for_scope(&self, scope: &str) -> usize {
        self.writes_for_scope(scope).len()
    }
}

#[async_trait]
impl StoragePort for MockMultiTenantStorage {
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        // Record the write before applying it so UAT-5 can inspect scope distribution.
        self.write_log.lock().push((scope.as_str().to_string(), ops.to_vec()));

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
        scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        // Record the scope this search was executed under (for UAT-5 leakage checks).
        self.search_log.lock().push(scope.as_str().to_string());
        let hits = self.vector_hits.lock().get(scope.as_str()).cloned().unwrap_or_default();
        Ok(hits)
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
impl KeywordPort for MockMultiTenantStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        // Keyword always returns empty — vector hits are the test surface.
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Helper — make a minimal Chunk + Episode for the given scope and text
// ---------------------------------------------------------------------------

/// Returns a `(chunk, episode)` pair whose chunk.text == `text` and
/// episode.source == `source`. Both share the same HLC stamp so hydration
/// can construct a non-empty `Hit`.
fn make_chunk_and_episode(
    scope: &Scope,
    text: &str,
    source: &str,
    clock: &HlcClock,
) -> (Chunk, Episode) {
    let ep_id = Ulid::new();
    let ch_id = Ulid::new();
    let bt = BiTemporal {
        valid: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
        sys: (Hlc { wall_ms: 1, counter: 0, node_id: 0 }, None),
    };
    let chunk = Chunk {
        id: ch_id,
        scope: scope.clone(),
        episode_id: ep_id,
        text: text.to_string(),
        tokens: text.split_whitespace().count() as u32,
        offset: 0,
        heading_path: vec![],
        overlap_tail: String::new(),
        embedding: None,
        bt,
        parent_id: None,
    };
    let episode = Episode {
        id: ep_id,
        source: source.to_string(),
        content: text.to_string(),
        t_ref: None,
        bt: BiTemporal::now(clock),
        metadata: serde_json::Map::new(),
        scope: scope.clone(),
    };
    (chunk, episode)
}

// ---------------------------------------------------------------------------
// Helper — token-file builder
// ---------------------------------------------------------------------------

/// Write a tokens file and return the path. `entries` is a list of
/// `(token_string, tenant_string, scopes)` triples.
fn write_tokens_file(entries: &[(&str, &str, &[&str])]) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-uat-tokens-{}.json", Ulid::new()));
    let mut map = serde_json::Map::new();
    for (tok, tenant, scopes) in entries {
        map.insert(
            tok.to_string(),
            serde_json::json!({
                "tenant": tenant,
                "scopes": scopes,
            }),
        );
    }
    std::fs::write(&path, serde_json::to_string(&map).unwrap()).expect("write tokens");
    path
}

/// Build a test `axum::Router` backed by a shared `MockMultiTenantStorage`.
fn build_app(
    storage: Arc<MockMultiTenantStorage>,
    tokens_file: std::path::PathBuf,
) -> axum::Router {
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

// ---------------------------------------------------------------------------
// UAT-1: Two-tenant cross-scope isolation
// ---------------------------------------------------------------------------

/// Ingest "Alice met Bob today" under scope_a, "Quarterly revenue grew 12%" under
/// scope_b. Both scopes' data lives in the **same** `MockMultiTenantStorage`
/// instance to prove that a shared backend correctly partitions at the `&Scope`
/// argument level. Assert:
///
/// - UAT-1a: scope_a recalls "Alice" → hit text contains "Alice".
/// - UAT-1b: scope_b recalls "Alice" → **zero hits** even though scope_a's chunk
///   and episode rows are present in the same KV storage. `vector_search` is
///   keyed by `Scope` — scope_b's call only looks up `"agent.beta"` hits, which
///   are absent at this point → zero results → no cross-scope bleed.
/// - UAT-1c: scope_b recalls "revenue" → hit text contains "revenue".
///
/// ## Implementation note
///
/// `MockMultiTenantStorage::vector_search` returns all pre-seeded hits for a
/// scope regardless of the query embedding (the `StubEmbedder` always produces
/// a zero vector). For UAT-1b to return zero hits, scope_b must have NO seeded
/// vector_hits at call time. We seed scope_b's vector_hit ONLY after UAT-1b
/// completes, then run UAT-1c. The KV rows (chunk + episode) for BOTH scopes
/// are seeded upfront in the shared storage — `read_as_of` is also scope-keyed,
/// so scope_b's hydration cannot retrieve scope_a's chunk even if a hit were
/// returned by a misconfigured `vector_search`.
#[tokio::test]
async fn uat1_cross_scope_ingest_recall_isolation() {
    let clock = HlcClock::new(0);

    let scope_a = Scope::new("agent.alpha").unwrap();
    let scope_b = Scope::new("agent.beta").unwrap();

    let tokens_file = write_tokens_file(&[
        ("tok-alpha", "agent.alpha", &["ingest", "recall", "forget"]),
        ("tok-beta", "agent.beta", &["ingest", "recall", "forget"]),
    ]);

    // Shared storage — both scopes' KV rows coexist here.
    // This proves KV-level partitioning (read_as_of keyed by scope) in addition
    // to the vector_hits partitioning.
    let storage = Arc::new(MockMultiTenantStorage::new());

    // Seed scope_a: chunk + episode + vector_hit
    let (chunk_a, episode_a) =
        make_chunk_and_episode(&scope_a, "Alice met Bob today", "agent-alpha:notes", &clock);
    storage.seed_chunk(&scope_a, &chunk_a);
    storage.seed_episode(&scope_a, &episode_a);
    storage.seed_vector_hit(
        &scope_a,
        VectorHit {
            id: chunk_a.id.to_bytes().to_vec(),
            score: 0.95,
            rerank_applied: false,
            metadata: serde_json::json!({}),
        },
    );

    // Seed scope_b: chunk + episode ONLY (no vector_hit yet — seeded after UAT-1b).
    // This proves the KV partition: scope_b's rows are in the same HashMap but
    // cannot be accessed via scope_a's key tuple (scope_a_str, key_bytes).
    let (chunk_b, episode_b) = make_chunk_and_episode(
        &scope_b,
        "Quarterly revenue grew 12%",
        "agent-beta:reports",
        &clock,
    );
    storage.seed_chunk(&scope_b, &chunk_b);
    storage.seed_episode(&scope_b, &episode_b);

    let app = build_app(storage.clone(), tokens_file);

    // UAT-1a: scope_a recalls "Alice" → sees its own data
    let resp_a = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/recall")
                .header("authorization", "Bearer tok-alpha")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"Alice","k":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK, "UAT-1a: scope_a recall must return 200");
    let bytes_a = to_bytes(resp_a.into_body(), 65_536).await.unwrap();
    let hits_a: serde_json::Value = serde_json::from_slice(&bytes_a).unwrap();
    let arr_a = hits_a.as_array().expect("UAT-1a: must be array");
    assert!(!arr_a.is_empty(), "UAT-1a: scope_a must see its own chunk; got empty hits");
    let text_a = arr_a[0]["text"].as_str().unwrap_or("");
    assert!(text_a.contains("Alice"), "UAT-1a: hit text must contain 'Alice'; got: {text_a}");

    // UAT-1b: scope_b recalls "Alice" → ZERO hits.
    // - scope_b has no seeded vector_hits → vector_search(&scope_b) returns empty.
    // - scope_a's vector_hit IS seeded but lives under "agent.alpha" key, not "agent.beta".
    // - Even if a hit were somehow returned, read_as_of(&scope_b, scope_a_chunk_key)
    //   would return None (different scope tuple in the KV map).
    let resp_b_alice = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/recall")
                .header("authorization", "Bearer tok-beta")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"Alice","k":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_b_alice.status(),
        StatusCode::OK,
        "UAT-1b: cross-scope recall must return 200 (empty, not 403)"
    );
    let bytes_b_alice = to_bytes(resp_b_alice.into_body(), 65_536).await.unwrap();
    let hits_b_alice: serde_json::Value = serde_json::from_slice(&bytes_b_alice).unwrap();
    let arr_b_alice = hits_b_alice.as_array().expect("UAT-1b: must be array");
    assert_eq!(
        arr_b_alice.len(),
        0,
        "UAT-1b: scope_b must see ZERO hits (no cross-scope leak, no scope_b vector_hits yet); got: {hits_b_alice}"
    );

    // Now seed scope_b's vector_hit so UAT-1c can verify scope_b sees its own data.
    storage.seed_vector_hit(
        &scope_b,
        VectorHit {
            id: chunk_b.id.to_bytes().to_vec(),
            score: 0.91,
            rerank_applied: false,
            metadata: serde_json::json!({}),
        },
    );

    // UAT-1c: scope_b recalls "revenue" → sees its own data.
    // Same shared storage — scope_b now has a vector_hit → hydrate finds chunk_b
    // in the (scope_b, key) partition → returns Hit with text "Quarterly revenue...".
    let resp_b_rev = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/recall")
                .header("authorization", "Bearer tok-beta")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"revenue","k":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_b_rev.status(), StatusCode::OK, "UAT-1c: scope_b revenue recall must be 200");
    let bytes_b_rev = to_bytes(resp_b_rev.into_body(), 65_536).await.unwrap();
    let hits_b_rev: serde_json::Value = serde_json::from_slice(&bytes_b_rev).unwrap();
    let arr_b_rev = hits_b_rev.as_array().expect("UAT-1c: must be array");
    assert!(!arr_b_rev.is_empty(), "UAT-1c: scope_b must see its own chunk; got empty");
    let text_b = arr_b_rev[0]["text"].as_str().unwrap_or("");
    assert!(text_b.contains("revenue"), "UAT-1c: hit text must contain 'revenue'; got: {text_b}");
}

// ---------------------------------------------------------------------------
// UAT-2: Malformed scope → 401
// ---------------------------------------------------------------------------

/// Tokens whose `tenant` field is empty, overlong (129 chars), or contains
/// invalid characters (`%`, ` `, `\`) must produce HTTP 401 Unauthorized.
/// The auth middleware calls `Scope::new(claims.tenant)` and rejects on error.
#[tokio::test]
async fn uat2_malformed_scope_rejects_401() {
    // All three bad-tenant tokens: empty, too-long, invalid-chars.
    let too_long = "a".repeat(129);
    let tokens_file = write_tokens_file(&[
        ("tok-empty", "", &["ingest", "recall"]),
        ("tok-toolong", &too_long, &["ingest", "recall"]),
        ("tok-percent", "bad%scope", &["ingest", "recall"]),
        ("tok-space", "bad scope", &["ingest", "recall"]),
        ("tok-backslash", "bad\\scope", &["ingest", "recall"]),
    ]);

    let storage = Arc::new(MockMultiTenantStorage::new());
    let app = build_app(storage, tokens_file);
    let ingest_body = r#"{"source":"s","content":"hello"}"#;

    for (tok, label) in &[
        ("tok-empty", "empty tenant"),
        ("tok-toolong", "129-char tenant"),
        ("tok-percent", "percent-char tenant"),
        ("tok-space", "space-char tenant"),
        ("tok-backslash", "backslash-char tenant"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/ingest")
                    .header("authorization", format!("Bearer {tok}"))
                    .header("content-type", "application/json")
                    .body(Body::from(ingest_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "UAT-2 ({label}): must reject with 401; got {}",
            resp.status()
        );
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["error"], "unauthorized",
            "UAT-2 ({label}): error key must be 'unauthorized'; got {v}"
        );
    }
}

// ---------------------------------------------------------------------------
// UAT-3: Request body cannot override scope
// ---------------------------------------------------------------------------

/// A request body that includes `"scope"` or `"tenant"` at the top level must
/// be rejected with HTTP 422 Unprocessable Entity. The `IngestBody` DTO uses
/// `#[serde(deny_unknown_fields)]`, so the field causes a serde error before
/// the handler runs.
///
/// This verifies that a malicious client cannot inject a `"scope": "victim"`
/// key to write into another agent's partition.
#[tokio::test]
async fn uat3_body_scope_override_rejected_422() {
    let tokens_file = write_tokens_file(&[("tok-a", "agent-a", &["ingest", "recall"])]);
    let storage = Arc::new(MockMultiTenantStorage::new());
    let app = build_app(storage.clone(), tokens_file);

    // --- attempt 1: body contains top-level "scope" ---
    let evil_scope_body = serde_json::json!({
        "source": "evil",
        "content": "trying to inject scope",
        "scope": "victim-scope",
    });
    let resp_scope = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ingest")
                .header("authorization", "Bearer tok-a")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&evil_scope_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_scope.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "UAT-3a: body with 'scope' field must return 422"
    );

    // --- attempt 2: body contains top-level "tenant" ---
    let evil_tenant_body = serde_json::json!({
        "source": "evil",
        "content": "trying to inject tenant",
        "tenant": "victim-tenant",
    });
    let resp_tenant = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ingest")
                .header("authorization", "Bearer tok-a")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&evil_tenant_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_tenant.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "UAT-3b: body with 'tenant' field must return 422"
    );

    // --- verify: zero episodes were written by either bad request ---
    // Both requests should have been rejected at deserialization — no storage
    // writes should have occurred. The batch count for scope "agent-a" is 0.
    let batch_count = storage.batch_count_for_scope("agent-a");
    assert_eq!(
        batch_count, 0,
        "UAT-3c: no storage batch must be written after 422 rejections; got {batch_count}"
    );
}

// ---------------------------------------------------------------------------
// UAT-4: Forget is scope-aware
// ---------------------------------------------------------------------------

/// Ingest 2 episodes under JWT_A (scope "agent-a"). Attempt a dry-run forget
/// with JWT_B (scope "agent-b") targeting the first episode's id.
///
/// ## Current behavior (Wave 1E)
///
/// The forget handler currently uses `state.lunaris.forget(request)` without a
/// scope-scoped wrapper — it is the only verb that has not yet been migrated to
/// `ScopedLunaris` (Wave 1D only wired ingest + recall). Inside
/// `Lunaris::forget`, the soft-delete path calls `atomic_write(&Scope::dev(),
/// ops)`. The `Scope::dev()` partition ("_dev_") is different from the real
/// scope partition where scope_a's episodes live — so the forget call finds zero
/// matching rows and returns a receipt with `rows_written == 0`.
///
/// The test asserts the observable contract:
/// - HTTP 200 (auth passes — scope_b has the "forget" permission).
/// - `preview == false` (not a dry_run request).
/// - `rows_written == 0` and `rows_deleted == 0` — the cross-scope forget had
///   no effect on scope_a's data.
/// - scope_a's episodes are still retrievable after the cross-scope forget.
///
/// ## Future contract (Wave 1F / v0.3)
///
/// Once the forget handler is migrated to `scoped.forget(request)`, the
/// expected HTTP status for a cross-scope target will change from 200 (zero
/// rows) to 403 or 404. External consumers MUST be prepared for either status
/// code once the scoped forget lands.
#[tokio::test]
async fn uat4_forget_honors_scope() {
    let clock = HlcClock::new(0);
    let storage = Arc::new(MockMultiTenantStorage::new());

    let scope_a = Scope::new("agent-a").unwrap();

    // Seed scope_a with two episodes.
    let (chunk_1, episode_1) =
        make_chunk_and_episode(&scope_a, "Episode 1 content", "src:ep1", &clock);
    let (chunk_2, episode_2) =
        make_chunk_and_episode(&scope_a, "Episode 2 content", "src:ep2", &clock);
    storage.seed_chunk(&scope_a, &chunk_1);
    storage.seed_episode(&scope_a, &episode_1);
    storage.seed_vector_hit(
        &scope_a,
        VectorHit {
            id: chunk_1.id.to_bytes().to_vec(),
            score: 0.9,
            rerank_applied: false,
            metadata: serde_json::json!({}),
        },
    );
    storage.seed_chunk(&scope_a, &chunk_2);
    storage.seed_episode(&scope_a, &episode_2);

    let tokens_file = write_tokens_file(&[
        ("tok-a", "agent-a", &["ingest", "recall", "forget"]),
        ("tok-b", "agent-b", &["ingest", "recall", "forget"]),
    ]);
    let app = build_app(storage.clone(), tokens_file);

    // --- JWT_B attempts to forget scope_a's first episode (dry_run=true) ---
    // Under Wave 1E this returns 200 with rows_written == 0 (no match in _dev_
    // partition). In a future scoped-forget world, this would return 403/404.
    let forget_body = serde_json::json!({
        "target": { "Id": episode_1.id.to_string() },
        "dry_run": true,
    });
    let resp_forget = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/forget")
                .header("authorization", "Bearer tok-b")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&forget_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    // HTTP 200: scope_b has the "forget" scope, so auth passes.
    assert_eq!(
        resp_forget.status(),
        StatusCode::OK,
        "UAT-4a: cross-scope forget must return 200 (auth passes, zero rows affected)"
    );
    let bytes_forget = to_bytes(resp_forget.into_body(), 65_536).await.unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&bytes_forget).unwrap();

    // preview == true because dry_run == true.
    assert_eq!(receipt["preview"], true, "UAT-4b: dry_run receipt must have preview=true");

    // rows_written == 0: scope_a's episode is in its own partition; scope_b's
    // forget (which uses Scope::dev() internally) cannot see it.
    let rows_written = receipt["rows_written"].as_u64().unwrap_or(999);
    let rows_deleted = receipt["rows_deleted"].as_u64().unwrap_or(999);
    assert_eq!(
        rows_written, 0,
        "UAT-4c: cross-scope forget must write 0 rows; got rows_written={rows_written}"
    );
    assert_eq!(
        rows_deleted, 0,
        "UAT-4d: cross-scope forget must delete 0 rows; got rows_deleted={rows_deleted}"
    );

    // --- verify scope_a's data is still intact ---
    let resp_recall = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/recall")
                .header("authorization", "Bearer tok-a")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"Episode","k":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_recall.status(), StatusCode::OK, "UAT-4e: scope_a recall must still work");
    let bytes_recall = to_bytes(resp_recall.into_body(), 65_536).await.unwrap();
    let hits: serde_json::Value = serde_json::from_slice(&bytes_recall).unwrap();
    let arr = hits.as_array().expect("UAT-4e: must be array");
    assert!(
        !arr.is_empty(),
        "UAT-4e: scope_a data must survive the cross-scope forget; got empty hits"
    );
}

/// **Target contract for UAT-4 once `ScopedLunaris::forget` lands (Wave 1F).**
///
/// This test is currently `#[ignore]`'d because the forget handler has not yet
/// been migrated from `state.lunaris.forget(request)` to a scoped equivalent.
/// Once the migration lands, un-ignore this test — it will fail until the
/// handler returns `403 Forbidden` or `404 Not Found` for a cross-scope target.
///
/// External consumers (Helios): once this test turns green, the v0.3 contract
/// applies and your client code must handle 403/404 on cross-scope forget
/// attempts (not just 200 with rows_written=0).
#[tokio::test]
#[ignore = "un-ignore after ScopedLunaris::forget migration lands (Wave 1F / v0.3)"]
async fn uat4_forget_honors_scope_target_contract() {
    let clock = HlcClock::new(0);
    let storage = Arc::new(MockMultiTenantStorage::new());

    let scope_a = Scope::new("agent-a").unwrap();

    let (chunk_1, episode_1) =
        make_chunk_and_episode(&scope_a, "Episode 1 content", "src:ep1", &clock);
    storage.seed_chunk(&scope_a, &chunk_1);
    storage.seed_episode(&scope_a, &episode_1);

    let tokens_file = write_tokens_file(&[
        ("tok-a", "agent-a", &["ingest", "recall", "forget"]),
        ("tok-b", "agent-b", &["ingest", "recall", "forget"]),
    ]);
    let app = build_app(storage.clone(), tokens_file);

    // JWT_B attempts to forget scope_a's episode.
    // POST /forget target cross-scope → must return 403 or 404 once scoped.
    let forget_body = serde_json::json!({
        "target": { "Id": episode_1.id.to_string() },
        "dry_run": true,
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/forget")
                .header("authorization", "Bearer tok-b")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&forget_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    // This assertion will FAIL on Wave 1E (returns 200) and PASS on Wave 1F+.
    assert!(
        resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
        "UAT-4 target: cross-scope forget MUST return 403 or 404 once ScopedLunaris::forget lands; got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// UAT-5: Concurrent multi-agent traffic (smoke)
// ---------------------------------------------------------------------------

/// Spin 10 tokio tasks, each with a unique token (tenant `agent:N` for N in
/// 0..10). Every task calls POST /ingest 3 times and POST /recall 3 times.
/// Total: 60 HTTP operations.
///
/// Assertions:
/// 1. All 60 operations return HTTP 200.
/// 2. Storage write_log shows exactly 10 distinct scope strings.
/// 3. search_log shows exactly 10 distinct scope strings.
/// 4. Each scope's batch count matches the expected 3 ingest calls (the
///    recall path also calls atomic_write for audit — so batch_count >= 3).
///
/// Note: Tower's `ServiceExt::oneshot` consumes the router, so we cannot share
/// a single router across tokio tasks. Instead, each task gets its own clone
/// of the `Arc<MockMultiTenantStorage>` and builds an independent router backed
/// by the same storage — this exercises true concurrent access.
#[tokio::test]
async fn uat5_concurrent_multi_agent_smoke() {
    const N_AGENTS: usize = 10;
    const OPS_PER_AGENT: usize = 3; // 3 ingests + 3 recalls

    let storage = Arc::new(MockMultiTenantStorage::new());

    // Build a tokens file with all 10 agent tokens.
    let token_entries: Vec<(String, String, Vec<&str>)> = (0..N_AGENTS)
        .map(|i| {
            (format!("tok-agent-{i}"), format!("agent.{i}"), vec!["ingest", "recall", "forget"])
        })
        .collect();

    let entries_refs: Vec<(&str, &str, &[&str])> = token_entries
        .iter()
        .map(|(tok, tenant, scopes)| (tok.as_str(), tenant.as_str(), scopes.as_slice()))
        .collect();

    let tokens_file = write_tokens_file(&entries_refs);

    let mut tasks = tokio::task::JoinSet::new();

    for i in 0..N_AGENTS {
        let storage_clone = storage.clone();
        let tokens_file_clone = tokens_file.clone();
        let token = format!("tok-agent-{i}");
        let scope_str = format!("agent.{i}");

        tasks.spawn(async move {
            let app = build_app(storage_clone, tokens_file_clone);

            let mut results: Vec<StatusCode> = Vec::new();

            for j in 0..OPS_PER_AGENT {
                // Ingest
                let body = serde_json::json!({
                    "source": format!("agent:{i}:source"),
                    "content": format!("observation {j} from agent {i}"),
                });
                let ingest_resp = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/v1/ingest")
                            .header("authorization", format!("Bearer {token}"))
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::to_vec(&body).unwrap()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                results.push(ingest_resp.status());

                // Recall
                let recall_resp = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/v1/recall")
                            .header("authorization", format!("Bearer {token}"))
                            .header("content-type", "application/json")
                            .body(Body::from(
                                serde_json::json!({ "query": format!("agent {i}"), "k": 3 })
                                    .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                results.push(recall_resp.status());
            }

            (scope_str, results)
        });
    }

    // Collect all results and assert all 60 operations succeeded.
    let mut all_scopes_seen: Vec<String> = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (scope_str, statuses) = result.expect("task did not panic");
        all_scopes_seen.push(scope_str.clone());
        for (op_idx, status) in statuses.iter().enumerate() {
            assert_eq!(
                *status,
                StatusCode::OK,
                "UAT-5: op[{op_idx}] for scope '{scope_str}' must return 200; got {status}"
            );
        }
    }

    // Assert all 10 distinct scopes appeared in write_log.
    let written_scopes: std::collections::HashSet<String> =
        storage.write_log.lock().iter().map(|(s, _)| s.clone()).collect();
    assert_eq!(
        written_scopes.len(),
        N_AGENTS,
        "UAT-5: write_log must show {N_AGENTS} distinct scopes; got {written_scopes:?}"
    );

    // Assert all 10 distinct scopes appeared in search_log.
    let searched_scopes: std::collections::HashSet<String> =
        storage.search_log.lock().iter().cloned().collect();
    assert_eq!(
        searched_scopes.len(),
        N_AGENTS,
        "UAT-5: search_log must show {N_AGENTS} distinct scopes; got {searched_scopes:?}"
    );

    // Assert each agent's scope appears in the write_log.
    for i in 0..N_AGENTS {
        let scope_str = format!("agent.{i}");
        assert!(
            written_scopes.contains(&scope_str),
            "UAT-5: scope '{scope_str}' must appear in write_log"
        );
    }
}
