//! F16 — `mode=graph` must return hits from a graph corpus THIS TEST seeded.
//!
//! `recall_graph_mode.rs` drives `MockGraphStorage` for all four of its tests.
//! They pin the handler's composition and its 501 gate, and they are worth
//! keeping, but a canned `GraphResult` proves nothing about whether the graph
//! leg reaches real `GRAPH.QUERY` rows. This file closes that gap end to end:
//! HTTP -> handler -> `Graph::anchored` -> Moon -> hits.
//!
//! **The corpus is seeded here, on purpose.** The test this replaces was
//! deleted for asserting non-emptiness over an operator's ambient store, which
//! is satisfied by data the code under test never wrote.
//!
//! ## What the RED run found
//!
//! `mode=graph` could not return a hit at all, for two reasons that compose:
//!
//! 1. `Graph::anchored` returns `m.id_hex`, and `hydrate_mixed` resolves a
//!    candidate id as a chunk key or a fact key — never an entity key. So a
//!    traversal that lands on an ENTITY is dropped by design; only a FACT can
//!    become a hit.
//! 2. Fact graph nodes carried no `id_hex` property. Measured live against the
//!    traversal both ingest paths produce:
//!
//!    ```text
//!    headers=["id_hex", "id", "lbl"]
//!    row=[String("e3978876e0b27ee9…"), String("e3978876e0b27ee9…"), …]  <- entity
//!    row=[Null,                        String("01a0292f0a1bb481…"), …]  <- Fact
//!    ```
//!
//!    `RETURN m.id_hex` is NULL for exactly the node kind that could have
//!    hydrated, so `hex::decode("")` yielded an empty id and the candidate was
//!    dropped. The one thing the graph leg could contribute was the one thing
//!    the query could not name.
//!
//! On top of that, `ingest_structured` wrote no `Fact` graph node at all —
//! `ingest.rs` did, `structured_ingest.rs` did not — so the agent-supplied
//! path could never put a fact in the graph to begin with.
//!
//! ## Why a NoopEmbedder makes this discriminating
//!
//! `mode=graph` composes `Vector::new("chunks", 30) ∧ Graph::anchored(..)`
//! under RRF. With every embedding all-zero, the F22 write guard omits the
//! `vec` field entirely, so nothing this test seeds is in the KNN index and the
//! vector leg contributes NOTHING. Any hit that comes back therefore came from
//! the graph leg — which is exactly the claim under test.

#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use chrono::{TimeZone, Utc};
use lunaris::structured_ingest::{EntityInput, FactInput, RelationInput, StructuredIngest};
use lunaris::{EpisodeBuilder, Lunaris};
use lunaris_core::storage::types::CypherQuery;
use lunaris_core::{Embedder, NoopEmbedder, Scope};
use lunaris_test_harness::{TestStore, open_test_engine_with_embedder};
use tower::ServiceExt;

/// The tenant the test token maps to; every seeded row and every request share
/// it, because the JWT claim is the only source of truth for the scope.
const TENANT: &str = "t-1";

fn write_test_tokens_file() -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lunaris-graph-live-tokens-{}.json", ulid::Ulid::new()));
    let body = serde_json::json!({
        "tok-all": { "tenant": TENANT, "scopes": ["ingest", "recall", "forget"] },
    });
    std::fs::write(&path, body.to_string()).expect("write tokens file");
    path
}

fn build_test_app_with(lunaris: Arc<Lunaris>) -> axum::Router {
    let cfg = lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://live".to_string(),
        tokens_file: write_test_tokens_file(),
        rate_per_second: 1000,
        rate_burst: 1000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        http_timeout_secs: 30,
        http_concurrency: 256,
        metrics_disabled: true,
    };
    lunaris_server::build(cfg, lunaris)
}

/// A real engine over an ephemeral Moon, or `None` with a loud reason.
///
/// The `TestStore` owns the Moon child process — the caller MUST bind it.
async fn live_engine(test: &str) -> Option<(Arc<Lunaris>, TestStore)> {
    if lunaris_test_harness::moon_binary().is_none() {
        eprintln!("{test}: no Moon binary (set MOON_TEST_BINARY); SKIP");
        return None;
    }
    let engine =
        open_test_engine_with_embedder(Arc::new(NoopEmbedder::new(768)) as Arc<dyn Embedder>).await;
    let (lunaris, store) = engine.into_parts();
    if !lunaris.storage().capabilities().graph_native {
        eprintln!("{test}: backend has no native graph; SKIP");
        return None;
    }
    Some((Arc::new(lunaris), store))
}

/// Alice knows Bob, as an entity pair, a relation, AND a fact.
///
/// The fact matters: a traversal that reaches only entities has nothing
/// hydratable to return, so a corpus without facts cannot exercise the path
/// under test even when everything works.
fn alice_knows_bob() -> StructuredIngest {
    let valid_from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
    let ent = |name: &str| EntityInput {
        name: name.into(),
        entity_type: "Person".into(),
        aliases: vec![],
        confidence: 0.95,
        valid_from,
        valid_to: None,
        embedding: None,
    };
    StructuredIngest::new(EpisodeBuilder::new(
        "chat:f16/turn-1",
        "Quarterly planning wrapped up ahead of schedule.",
    ))
    .with_entities(vec![ent("Alice"), ent("Bob")])
    .with_relations(vec![RelationInput {
        subject_name: "Alice".into(),
        subject_type: "Person".into(),
        predicate: "KNOWS".into(),
        object_name: "Bob".into(),
        object_type: "Person".into(),
        confidence: 0.9,
        valid_from,
        valid_to: None,
    }])
    .with_facts(vec![FactInput {
        fact_text: "Alice knows Bob.".into(),
        subject_name: "Alice".into(),
        subject_type: "Person".into(),
        predicate: "KNOWS".into(),
        object_name: "Bob".into(),
        object_type: "Person".into(),
        confidence: 0.9,
        valid_from,
        valid_to: None,
    }])
}

fn alice_id() -> lunaris_extract::EntityId {
    lunaris_extract::EntityId::from_name_and_type("Alice", "Person")
}

fn bob_id() -> lunaris_extract::EntityId {
    lunaris_extract::EntityId::from_name_and_type("Bob", "Person")
}

async fn recall_graph(app: &axum::Router, query: &str) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({ "query": query, "k": 10, "mode": "graph" });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/recall")
        .header("authorization", "Bearer tok-all")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    let resp = app.clone().oneshot(req).await.expect("recall");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Positive control: the corpus really did land in the graph.
///
/// Without this, "mode=graph returned hits" and "mode=graph returned nothing
/// because nothing was seeded" look identical from the HTTP side.
#[tokio::test]
async fn the_seeded_corpus_reaches_the_graph() {
    let Some((lunaris, _store)) = live_engine("the_seeded_corpus_reaches_the_graph").await else {
        return;
    };
    let scope = Scope::new(TENANT).expect("scope");
    lunaris.scoped(scope.clone()).ingest_structured(alice_knows_bob()).await.expect("ingest");

    let q = CypherQuery {
        graph: "lunaris_graph".into(),
        cypher: "UNWIND $ids AS sid MATCH (n)-[*1..2]-(m) WHERE n.id_hex = sid \
                 RETURN m.id_hex AS id LIMIT 25"
            .into(),
        params: serde_json::from_value(serde_json::json!({ "ids": [format!("{}", alice_id())] }))
            .expect("params"),
    };
    let result = lunaris.storage().graph_traverse(&scope, &q, None).await.expect("graph traverse");
    assert!(!result.rows.is_empty(), "the anchor entity reached nothing — the seed never landed");
    assert!(
        result.rows.iter().all(|r| r.first().is_some_and(|v| v.is_string())),
        "every reachable node must answer to `id_hex`; a NULL here is a node kind the \
         retrieval Cypher cannot name, and its candidate is silently dropped. rows={:?}",
        result.rows
    );

    // The fact is the ONLY reachable thing that can hydrate into a hit, so the
    // traversal must actually reach it. Its id is deterministic, which is why
    // this can be an identity assertion rather than a count.
    let want = ulid::Ulid::from_bytes(
        lunaris_extract::types::FactId::from_triple(alice_id(), "KNOWS", bob_id()).0,
    );
    let want_hex = want.to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
    let got: Vec<&str> = result.rows.iter().filter_map(|r| r.first()?.as_str()).collect();
    assert!(
        got.contains(&want_hex.as_str()),
        "the seeded fact is not reachable from its own subject entity; \
         ingest_structured wrote it to KV but not to the graph. want={want_hex} got={got:?}"
    );
}

/// The finding in one assertion: seed a graph, ask for it by name, get hits.
#[tokio::test]
async fn mode_graph_returns_hits_from_a_corpus_this_test_seeded() {
    let Some((lunaris, _store)) = live_engine("mode_graph_returns_hits_from_a_corpus").await else {
        return;
    };
    let scope = Scope::new(TENANT).expect("scope");
    lunaris.scoped(scope).ingest_structured(alice_knows_bob()).await.expect("ingest");

    let app = build_test_app_with(lunaris);
    let (status, body) = recall_graph(&app, "who is Alice").await;

    assert_eq!(status, StatusCode::OK, "mode=graph must not 501 on a graph-native backend: {body}");
    let hits = body.as_array().expect("recall returns an array of hits");
    assert!(
        !hits.is_empty(),
        "mode=graph returned no hits over a corpus this test seeded; \
         the graph leg never reached a hydratable GRAPH.QUERY row. body={body}"
    );
}
