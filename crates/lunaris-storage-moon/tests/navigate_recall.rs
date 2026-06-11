//! ADD task `ft-navigate-recall` — live-Moon navigate suite
//! (contract FROZEN @ v1, 2026-06-11). Gated behind `moon-it` + `MOON_URL`.
//!
//! All graph/vector seeding goes through the PRODUCTION write path
//! (`StoragePort::atomic_write` with the same `WriteOp`s `lunaris::ingest`
//! builds) — that is the Built ≠ wired discriminator: navigate only surfaces
//! graph-expanded hits if the production `GraphNode` writer registers the
//! `_key` linkage.

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{GraphDecay, NavigateSpec, WriteOp};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

// Match the server's existing 768-d indices — the Phase-22 dim guardrail
// rejects a mismatched handle at connect() and the graceful-skip would
// otherwise silently turn this whole suite into a no-op (observed live).
const DIM: usize = 768;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect_with_dim(&url(), DIM).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

fn fresh_scope() -> Scope {
    Scope::new(format!("nav-{}", Ulid::new().to_string().to_lowercase())).expect("valid scope")
}

/// Deterministic 16-byte entity id from a marker byte.
fn eid(marker: u8) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[0] = marker;
    id[15] = marker;
    id
}

/// DIM-d embedding with two controlled components.
fn emb(x: f32, y: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = x;
    v[1] = y;
    v
}

fn node(id: Vec<u8>, name: &str) -> WriteOp {
    WriteOp::GraphNode {
        graph: "lunaris_graph".into(),
        id: id.clone(),
        label: "Person".into(),
        props: json!({
            "id_hex": hex::encode(&id),
            "name": name,
            "type": "Person",
            "aliases": [name.to_lowercase()],
            "confidence": 0.9,
        }),
    }
}

fn vector(id: Vec<u8>, name: &str, e: Vec<f32>) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "entities".into(),
        id,
        embedding: e,
        metadata: json!({"entity_type": "Person", "name": name}),
    }
}

fn edge(src: Vec<u8>, dst: Vec<u8>) -> WriteOp {
    WriteOp::GraphEdge {
        graph: "lunaris_graph".into(),
        src,
        dst,
        rel: "KNOWS".into(),
        props: json!({"confidence": 0.8}),
    }
}

/// Seed the discriminator fixture through ONE production atomic_write:
/// A near the query; D, E mid-distance vector distractors (isolated nodes);
/// B, C vector-far but graph-linked A->B->C.
async fn seed(moon: &MoonStorage, scope: &Scope) {
    let ops = vec![
        node(eid(1), "A"),
        vector(eid(1), "A", emb(1.0, 0.0)),
        node(eid(2), "B"),
        vector(eid(2), "B", emb(0.0, 10.0)),
        node(eid(3), "C"),
        vector(eid(3), "C", emb(10.0, 10.0)),
        node(eid(4), "D"),
        vector(eid(4), "D", emb(0.0, 5.0)),
        node(eid(5), "E"),
        vector(eid(5), "E", emb(5.0, 0.0)),
        edge(eid(1), eid(2)),
        edge(eid(2), eid(3)),
    ];
    moon.atomic_write(scope, &ops).await.expect("production atomic_write");
}

fn query() -> Vec<f32> {
    emb(1.0, 0.01)
}

fn ids_of_vector_hits(hits: &[lunaris_core::storage::types::VectorHit]) -> Vec<Vec<u8>> {
    hits.iter().map(|h| h.id.clone()).collect()
}

/// §2 DISCRIMINATOR — Navigate surfaces the graph-linked entity B that plain
/// vector recall at the same k misses; proves the production GraphNode writer
/// registered the `_key` linkage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigate_surfaces_graph_linked_entity() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;

    let plain = moon
        .vector_search(&scope, "entities", &query(), 3, None, None, false)
        .await
        .expect("plain vector recall");
    let plain_ids = ids_of_vector_hits(&plain);
    assert!(
        !plain_ids.contains(&eid(2)),
        "plain vector recall at k=3 must NOT reach the far entity B; got {plain_ids:?}"
    );
    assert!(plain_ids.contains(&eid(1)), "A must be the top vector hit");

    let spec = NavigateSpec::new(2).expect("valid hops");
    let nav = moon
        .vector_navigate(&scope, "entities", &query(), 3, &spec)
        .await
        .expect("navigate recall");
    let b_hit = nav
        .iter()
        .find(|h| h.id == eid(2))
        .unwrap_or_else(|| panic!("navigate must surface B via the graph; got {nav:?}"));
    assert!(b_hit.hop_depth >= 1, "B is a graph-expanded hit, got hop_depth {}", b_hit.hop_depth);
    assert!(nav.iter().any(|h| h.id == eid(1)), "A must still be present");
}

/// §2 idempotence — re-writing the same EntityId must not duplicate the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reingest_is_idempotent() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;
    // Second production write of entity A with refreshed props.
    let mut refreshed = node(eid(1), "A");
    if let WriteOp::GraphNode { props, .. } = &mut refreshed {
        props["confidence"] = json!(0.95);
    }
    moon.atomic_write(&scope, &[refreshed]).await.expect("re-ingest write");

    let q = lunaris_core::storage::types::CypherQuery {
        graph: String::new(),
        cypher: format!("MATCH (n:Person) WHERE n.id = '{}' RETURN id(n)", hex::encode(eid(1))),
        params: Default::default(),
    };
    let result = moon.graph_traverse(&scope, &q, None).await.expect("count nodes");
    assert_eq!(result.rows.len(), 1, "exactly one node for the re-ingested id; got {result:?}");
}

/// §2 graph-less index — chunks docs have no graph nodes; navigate degrades
/// to KNN-only server-side (hop_depth all 0), never an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigate_graphless_index_knn_only() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    let ops = vec![
        WriteOp::VectorUpsert {
            index: "chunks".into(),
            id: eid(7),
            embedding: emb(1.0, 0.0),
            metadata: json!({"text": "near chunk", "source": "test"}),
        },
        WriteOp::VectorUpsert {
            index: "chunks".into(),
            id: eid(8),
            embedding: emb(0.0, 1.0),
            metadata: json!({"text": "far chunk", "source": "test"}),
        },
    ];
    moon.atomic_write(&scope, &ops).await.expect("chunk writes");

    let spec = NavigateSpec::new(2).expect("valid hops");
    let nav = moon
        .vector_navigate(&scope, "chunks", &query(), 2, &spec)
        .await
        .expect("graph-less navigate must not error");
    assert!(!nav.is_empty(), "KNN hits must come back");
    assert!(
        nav.iter().all(|h| h.hop_depth == 0),
        "no chunk graph nodes -> hop_depth must be 0 everywhere; got {nav:?}"
    );
}

/// §2 decay composition — DECAY on the discovery edge worsens (never improves)
/// the graph-expanded hit's final_score and the query still executes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigate_decay_composes() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;

    let plain_spec = NavigateSpec::new(2).expect("valid hops");
    let nav_plain =
        moon.vector_navigate(&scope, "entities", &query(), 3, &plain_spec).await.expect("navigate");
    let b_plain = nav_plain
        .iter()
        .find(|h| h.id == eid(2))
        .unwrap_or_else(|| panic!("B expected without decay; got {nav_plain:?}"));

    let decay_spec = NavigateSpec::new(2)
        .expect("valid hops")
        .with_decay(GraphDecay::new(5.0).expect("valid λ"));
    let nav_decay = moon
        .vector_navigate(&scope, "entities", &query(), 3, &decay_spec)
        .await
        .expect("navigate with decay must execute");
    if let Some(b_decay) = nav_decay.iter().find(|h| h.id == eid(2)) {
        assert!(
            b_decay.final_score >= b_plain.final_score,
            "decay must not improve a graph-expanded hit: {} < {}",
            b_decay.final_score,
            b_plain.final_score
        );
    }
    // B falling out of top-k entirely under decay is also a legal outcome.
}

/// §1 ⚠ flag probe — record GRAPH.ADDNODE prop typing for float + array
/// props as observable behavior (resolves the freeze's lowest-confidence
/// flag with live evidence; passes on any typing, FAILS only if the write
/// or read errors).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn addnode_prop_typing_probe() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;

    let q = lunaris_core::storage::types::CypherQuery {
        graph: String::new(),
        cypher: format!(
            "MATCH (n:Person) WHERE n.id = '{}' RETURN n.confidence, n.aliases",
            hex::encode(eid(1))
        ),
        params: Default::default(),
    };
    let result = moon.graph_traverse(&scope, &q, None).await.expect("prop read-back");
    assert_eq!(result.rows.len(), 1, "node A must be readable; got {result:?}");
    eprintln!("ADDNODE_PROP_TYPING VERDICT: confidence/aliases read back as {:?}", result.rows[0]);
}

/// §2 capability — Moon reports native navigate support.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moon_reports_graph_navigate_native() {
    let Some(moon) = connect_or_skip().await else { return };
    assert!(moon.capabilities().graph_navigate_native, "Moon must report graph_navigate_native");
}
