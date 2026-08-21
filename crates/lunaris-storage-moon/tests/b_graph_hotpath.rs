//! moon-v051-perf-exploit W2 (graph hot path, Agent B) — live-Moon evidence
//! for Task 1 (point-lookup IndexScan + params) and Task 2 (single-round-trip
//! node upsert). Gated behind `moon-it` + `MOON_URL`.
//!
//! The FT.NAVIGATE merge gate for Task 2 (does the optimistic upsert still
//! let `GRAPH.ADDNODE` register the `key_to_node` mapping for brand-new
//! nodes?) is NOT duplicated here — `tests/navigate_recall.rs`'s
//! `navigate_surfaces_graph_linked_entity` and `reingest_is_idempotent`
//! already cover exactly that through the production `atomic_write` path and
//! were re-run green against the overhauled Moon (vendored @ c9508066) as
//! part of this wave.
//!
//! Run: `MOON_URL=moon://localhost:7802 cargo test -p lunaris-storage-moon \
//!        --features moon-it --test b_graph_hotpath -- --test-threads=1`
//! (single-threaded so the `total_commands_processed` round-trip counters in
//! `task2_*` aren't polluted by concurrent test traffic on the same server).

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{CypherQuery, WriteOp};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

mod common;
const DIM: usize = 768;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect_with_dim(&url(), DIM).await {
        Ok(s) => Some(s),
        Err(e) => {
            common::note_moon_unreachable(e);
            None
        }
    }
}

fn fresh_scope(tag: &str) -> Scope {
    Scope::new(format!("{tag}-{}", Ulid::new().to_string().to_lowercase())).expect("valid scope")
}

fn hex16(b: &[u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn node(id: &[u8; 16], name: &str, label: &str) -> WriteOp {
    WriteOp::GraphNode {
        graph: "ignored".into(),
        id: id.to_vec(),
        label: label.into(),
        props: json!({ "id_hex": hex16(id), "name": name, "type": label }),
        index_kind: "entities".into(),
    }
}

/// Task 1 — GRAPH.PROFILE evidence, straight against the Moon wire protocol
/// (no Lunaris code in the loop): the `WHERE n.id = '<hex>'` form Lunaris
/// used to emit MUST plan as a `NodeScan` (full label scan), while the
/// inline-property form `atomic.rs` now emits MUST plan as an `IndexScan`
/// (per-segment property index narrowing). Both must resolve to the SAME
/// single matching node — proves the v3-2 "Moon ignores inline-property
/// filters" hazard (PR #193) is fixed and the inline form is safe to rely on.
#[tokio::test]
async fn task1_indexscan_replaces_nodescan_for_point_lookup() {
    let Some(storage) = connect_or_skip().await else { return };
    let scope = fresh_scope("profile");
    let a = [0xaau8; 16];
    let b = [0xbbu8; 16];
    storage
        .atomic_write(&scope, &[node(&a, "Alice", "Person"), node(&b, "Bob", "Person")])
        .await
        .expect("seed two nodes");

    let scope_graph = lunaris_storage_moon::keyspace::graph_key(&scope);
    let mut typed = storage.client().typed();
    let conn = typed.inner_mut();

    let where_cypher = format!("MATCH (n:Person) WHERE n.id = '{}' RETURN n", hex16(&a));
    let where_profile: redis::Value = redis::cmd("GRAPH.PROFILE")
        .arg(scope_graph.as_str())
        .arg(&where_cypher)
        .query_async(conn)
        .await
        .expect("WHERE-form profile");
    let where_text = format!("{where_profile:?}");
    assert!(
        where_text.contains("NodeScan"),
        "WHERE-form point lookup must still plan as NodeScan (full label scan); got {where_text}"
    );
    assert!(
        !where_text.contains("IndexScan"),
        "WHERE-form point lookup must NOT get an IndexScan (Clause::Where always compiles to a \
         plain Filter per vendor/moon/src/graph/cypher/planner.rs::compile); got {where_text}"
    );

    let conn = typed.inner_mut();
    let inline_cypher = format!("MATCH (n:Person {{id: '{}'}}) RETURN n", hex16(&a));
    let inline_profile: redis::Value = redis::cmd("GRAPH.PROFILE")
        .arg(scope_graph.as_str())
        .arg(&inline_cypher)
        .query_async(conn)
        .await
        .expect("inline-form profile");
    let inline_text = format!("{inline_profile:?}");
    assert!(
        inline_text.contains("IndexScan"),
        "inline-property point lookup must plan as IndexScan; got {inline_text}"
    );

    // Functional correctness (not just the plan shape): the inline form must
    // resolve to EXACTLY the one matching node, not silently return every
    // node under the label (the pre-v3-2 hazard).
    let mut params = serde_json::Map::new();
    params.insert("id".into(), json!(hex16(&a)));
    let q = CypherQuery {
        graph: String::new(),
        cypher: "MATCH (n:Person {id: $id}) RETURN n.name".to_string(),
        params,
    };
    let result = storage.graph_traverse(&scope, &q, None).await.expect("inline+params traverse");
    assert_eq!(
        result.rows.len(),
        1,
        "inline-property + params point lookup must return exactly 1 row, not the whole label; \
         got {:?}",
        result.rows
    );
    assert_eq!(result.rows[0][0], json!("Alice"));
}

/// Task 2 — correctness merge gate for the optimistic single-round-trip
/// upsert. `total_commands_processed` (the obvious server-side round-trip
/// proxy) turned out NOT to be incremented for `GRAPH.*` dispatch at all —
/// live-probed directly against Moon: a bare `GRAPH.ADDNODE`/`GRAPH.QUERY`
/// outside any TXN left the counter unchanged, while a plain `SET` on the
/// same connection incremented it by exactly 1 every time. So the round-trip
/// count reduction is proven two other ways instead: (1) by construction —
/// `atomic.rs`'s `GraphNode` arm now issues exactly one `query_with_params`
/// call on the existing-node path before falling through, down from the old
/// `query_raw` (exists-check) + `query_raw` (update) pair, verifiable by
/// reading the diff; (2) empirically, via `examples/b_roundtrip_bench.rs`'s
/// wall-clock A/B (see this wave's summary for the recorded numbers). This
/// test instead pins CORRECTNESS: the optimistic update-first path must
/// still converge to exactly one node per id (no duplicate creation, no
/// silent no-op), across repeated create → update → update cycles.
#[tokio::test]
async fn task2_optimistic_upsert_stays_idempotent_across_create_and_update() {
    let Some(storage) = connect_or_skip().await else { return };
    let scope = fresh_scope("roundtrip");
    let id = [0x77u8; 16];

    // Create path (miss -> ADDNODE + follow-up SET).
    storage.atomic_write(&scope, &[node(&id, "Created", "Person")]).await.expect("create");
    // Two update-path rewrites in a row (optimistic SET hits both times).
    storage.atomic_write(&scope, &[node(&id, "Updated1", "Person")]).await.expect("update 1");
    storage.atomic_write(&scope, &[node(&id, "Updated2", "Person")]).await.expect("update 2");

    let q = CypherQuery {
        graph: String::new(),
        cypher: format!("MATCH (n:Person) WHERE n.id = '{}' RETURN n.name", hex16(&id)),
        params: Default::default(),
    };
    let result = storage.graph_traverse(&scope, &q, None).await.expect("post-write read-back");
    assert_eq!(
        result.rows.len(),
        1,
        "exactly one node must exist for this id after create+2 updates (no duplicate ADDNODE \
         on a hit, no missed update on a miss); got {:?}",
        result.rows
    );
    assert_eq!(
        result.rows[0][0],
        json!("Updated2"),
        "the optimistic SET must have actually applied the latest props; got {:?}",
        result.rows[0]
    );
}
