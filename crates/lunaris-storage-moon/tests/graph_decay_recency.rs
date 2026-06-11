//! ADD task `graph-decay-recency` — live-Moon decay traversal suite
//! (contract FROZEN @ v1, 2026-06-11). Gated behind `moon-it` + `MOON_URL`.
//!
//! Fixture mirrors Moon's own DECAY-01/02 cases
//! (vendor/moon/scripts/test-commands.sh:1918-1963): a STALE direct edge
//! (weight 1.0, written first) vs a FRESH detour (0.6 + 0.6, written after a
//! real wall-clock sleep). Without decay the cheaper direct path wins; with
//! λ=5 the older edge pays λ·age and the fresh detour wins. Nodes/edges are
//! seeded via raw GRAPH.ADDNODE/ADDEDGE (WEIGHT is an ADDEDGE argument, not a
//! Cypher property) — raw RESP in TESTS is fine; the typed-only rule is
//! queue.rs's contract.

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{CypherQuery, GraphDecay};
use lunaris_storage_moon::MoonStorage;
use std::time::Duration;
use ulid::Ulid;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

fn fresh_scope() -> Scope {
    Scope::new(format!("decay-{}", Ulid::new().to_string().to_lowercase())).expect("valid scope")
}

fn q(cypher: &str) -> CypherQuery {
    CypherQuery { graph: String::new(), cypher: cypher.into(), params: Default::default() }
}

const SHORTEST: &str =
    "MATCH p = shortestPath((a:Person {name: 'A'})-[*..5]->(c:Person {name: 'C'})) RETURN p";

/// Seed the stale-direct vs fresh-detour graph on the per-scope graph key.
/// Returns (node_id_a, node_id_b, node_id_c) as the server assigned them.
async fn seed_decay_graph(moon: &MoonStorage, scope: &Scope) -> (i64, i64, i64) {
    let graph = format!("lunaris_{}_graph", scope.as_str());
    let mut typed = moon.client().typed();
    let conn = typed.inner_mut();

    redis::cmd("GRAPH.CREATE").arg(&graph).query_async::<()>(conn).await.ok();
    let a: i64 = redis::cmd("GRAPH.ADDNODE")
        .arg(&graph)
        .arg("Person")
        .arg("name")
        .arg("A")
        .query_async(conn)
        .await
        .expect("ADDNODE A");
    let b: i64 = redis::cmd("GRAPH.ADDNODE")
        .arg(&graph)
        .arg("Person")
        .arg("name")
        .arg("B")
        .query_async(conn)
        .await
        .expect("ADDNODE B");
    let c: i64 = redis::cmd("GRAPH.ADDNODE")
        .arg(&graph)
        .arg("Person")
        .arg("name")
        .arg("C")
        .query_async(conn)
        .await
        .expect("ADDNODE C");

    // STALE direct edge first.
    redis::cmd("GRAPH.ADDEDGE")
        .arg(&graph)
        .arg(a)
        .arg(c)
        .arg("KNOWS")
        .arg("WEIGHT")
        .arg(1.0)
        .query_async::<i64>(conn)
        .await
        .expect("ADDEDGE A->C");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // FRESH detour edges after the age gap.
    redis::cmd("GRAPH.ADDEDGE")
        .arg(&graph)
        .arg(a)
        .arg(b)
        .arg("KNOWS")
        .arg("WEIGHT")
        .arg(0.6)
        .query_async::<i64>(conn)
        .await
        .expect("ADDEDGE A->B");
    redis::cmd("GRAPH.ADDEDGE")
        .arg(&graph)
        .arg(b)
        .arg(c)
        .arg("KNOWS")
        .arg("WEIGHT")
        .arg(0.6)
        .query_async::<i64>(conn)
        .await
        .expect("ADDEDGE B->C");

    (a, b, c)
}

/// Flatten every cell of a GraphResult into one string for path membership checks.
fn rows_blob(r: &lunaris_core::storage::types::GraphResult) -> String {
    serde_json::to_string(&r.rows).expect("rows serialize")
}

/// §2 DISCRIMINATOR — decay flips shortestPath from the stale direct route to
/// the fresh detour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_flips_shortest_path() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    let (_a, b, _c) = seed_decay_graph(&moon, &scope).await;

    let plain = moon
        .graph_traverse_decayed(&scope, &q(SHORTEST), None, None)
        .await
        .expect("shortestPath without decay");
    let blob = rows_blob(&plain);
    assert!(
        !blob.contains(&format!("{b}")),
        "without decay the direct A->C path must win (no B={b}); got rows: {blob}"
    );

    let decay = GraphDecay::new(5.0).expect("valid λ");
    let decayed = moon
        .graph_traverse_decayed(&scope, &q(SHORTEST), None, Some(&decay))
        .await
        .expect("shortestPath with decay");
    let blob = rows_blob(&decayed);
    assert!(
        blob.contains(&format!("{b}")),
        "with λ=5 the fresh detour via B={b} must win; got rows: {blob}"
    );
}

/// §2 delegation — decay None must match plain graph_traverse exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_none_matches_plain_traverse() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed_decay_graph(&moon, &scope).await;

    let direct = moon.graph_traverse(&scope, &q(SHORTEST), None).await.expect("plain traverse");
    let via_decayed = moon
        .graph_traverse_decayed(&scope, &q(SHORTEST), None, None)
        .await
        .expect("decayed(None) traverse");
    assert_eq!(direct.headers, via_decayed.headers, "headers must match");
    assert_eq!(direct.rows, via_decayed.rows, "rows must match (delegation)");
}

/// §2 composition — decay + $param + VALID_AT in one query line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_composes_with_params_and_valid_at() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed_decay_graph(&moon, &scope).await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let as_of = lunaris_core::hlc::Hlc::from_parts(now_ms, 0, 0);

    let mut query = q("MATCH (n:Person) WHERE n.name = $who RETURN n.name");
    query.params.insert("who".into(), serde_json::json!("A"));

    let decay = GraphDecay::new(5.0).expect("valid λ").with_time_weight(2.0).expect("valid w");
    let result = moon
        .graph_traverse_decayed(&scope, &query, Some(as_of), Some(&decay))
        .await
        .expect("decay + params + VALID_AT must execute");
    // Parseable result is the assertion; row content is backend-versioned.
    let _ = result.headers;
}

/// §1 Reject — decay on a write Cypher is rejected server-side and surfaces
/// as a Backend error (passthrough, not a panic).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_cypher_with_decay_rejected() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed_decay_graph(&moon, &scope).await;

    let decay = GraphDecay::new(1.0).expect("valid λ");
    let err = moon
        .graph_traverse_decayed(
            &scope,
            &q("MERGE (x:Person {name: 'Z'}) RETURN x"),
            None,
            Some(&decay),
        )
        .await
        .expect_err("decay on a write query must be rejected by Moon");
    assert!(matches!(err, StorageError::Backend(_)), "server rejection passthrough, got {err:?}");
}

/// §2 capability — Moon reports native decay support.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moon_reports_graph_decay_native() {
    let Some(moon) = connect_or_skip().await else { return };
    assert!(moon.capabilities().graph_decay_native, "Moon must report graph_decay_native=true");
}
