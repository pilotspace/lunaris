//! Live-Moon discriminating test for the graph-anchor fix (Memory Inspector
//! UAT, 2026-06-16). Gated behind `moon-it` + a reachable `MOON_URL`.
//!
//! Proves on REAL Moon that:
//! - the `WHERE n.id_hex = sid` form correctly constrains BOTH the anchor
//!   (`root`) and the hop bound (`depth`), and
//! - the inline-property anchor filter `(n {id_hex: sid})` NOW ALSO constrains
//!   correctly. History: pre-v3-2 Moon silently IGNORED inline-property
//!   filters — every node matched as an anchor and a depth-1 traversal
//!   wrongly reached 2-hop nodes (the bug the Inspector UAT surfaced,
//!   2026-06-16, workaround = WHERE form). Moon PR #193 (v0.4.0, vendored
//!   ≥ c9508066) fixed the planner to emit a Filter for inline properties,
//!   and the W2 graph hot-path work (moon-v051-perf-exploit, 2026-07-10)
//!   switched production point-lookups BACK to the inline form for
//!   IndexScan narrowing — so this test now pins the FIXED behavior of
//!   both forms as the regression guard.
//!
//! This is the regression guard the mock-based `graph_endpoint` suite could
//! never be — it records the CypherQuery string but never executes it.
//!
//! Run: `cargo test -p lunaris-storage-moon --features moon-it \
//!        --test graph_anchor_constrains` with `MOON_URL` pointing at a live
//! Moon (release build — the debug build lacks the graph/text-index features).

#![cfg(feature = "moon-it")]

use std::collections::BTreeSet;

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{CypherQuery, GraphResult, WriteOp};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

mod common;
fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            common::note_moon_unreachable(e);
            None
        }
    }
}

fn hex16(b: &[u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn node(id: &[u8; 16], name: &str, label: &str) -> WriteOp {
    WriteOp::GraphNode {
        graph: "ignored".into(), // atomic.rs routes to graph_key(scope), ignoring this
        id: id.to_vec(),
        label: label.into(),
        props: json!({ "id_hex": hex16(id), "name": name, "type": label }),
        index_kind: "entities".into(),
    }
}

fn edge(src: &[u8; 16], dst: &[u8; 16], rel: &str) -> WriteOp {
    WriteOp::GraphEdge {
        graph: "ignored".into(),
        src: src.to_vec(),
        dst: dst.to_vec(),
        rel: rel.into(),
        props: json!({}),
    }
}

/// The production anchored traversal template. Both forms must constrain
/// identically on Moon ≥ v0.4.0 (`inline=true` was broken pre-v3-2 and is
/// now the preferred IndexScan-narrowed form; `inline=false` is the
/// WHERE-clause era workaround, kept as a parity probe).
fn anchored(hops: usize, inline: bool) -> String {
    if inline {
        format!(
            "UNWIND $ids AS sid MATCH (n {{id_hex: sid}})-[*1..{hops}]-(m) \
             RETURN m.id_hex AS id, m.name AS name LIMIT $k"
        )
    } else {
        format!(
            "UNWIND $ids AS sid MATCH (n)-[*1..{hops}]-(m) WHERE n.id_hex = sid \
             RETURN m.id_hex AS id, m.name AS name LIMIT $k"
        )
    }
}

fn names(r: &GraphResult) -> BTreeSet<String> {
    let nc = r.headers.iter().position(|h| h == "name");
    r.rows
        .iter()
        .filter_map(|row| {
            nc.and_then(|i| row.get(i)).and_then(|v| v.as_str()).map(|s| s.to_string())
        })
        .collect()
}

#[tokio::test]
async fn graph_anchor_where_form_constrains_root_and_depth() {
    let Some(storage) = connect_or_skip().await else { return };
    let scope =
        Scope::new(format!("anchor-{}", Ulid::new().to_string().to_lowercase())).expect("scope");

    // Chain: Bob — Alice — Lunaris (Alice is the hub). Bob's only 1-hop
    // neighbor is Alice; Lunaris is 2 hops from Bob.
    let bob = [0xb0u8; 16];
    let alice = [0xa1u8; 16];
    let lun = [0x10u8; 16];
    storage
        .atomic_write(
            &scope,
            &[
                node(&bob, "Bob", "Person"),
                node(&alice, "Alice", "Person"),
                node(&lun, "Lunaris", "Project"),
                edge(&alice, &bob, "knows"),
                edge(&alice, &lun, "blocked_on"),
            ],
        )
        .await
        .expect("seed chain");

    let mut params = serde_json::Map::new();
    params.insert("ids".into(), json!([hex16(&bob)]));
    params.insert("k".into(), json!(30));
    let q = |cypher: String| CypherQuery { graph: String::new(), cypher, params: params.clone() };

    // FIXED (WHERE) form, root=Bob depth=1 → ONLY Alice.
    let w1 = storage.graph_traverse(&scope, &q(anchored(1, false)), None).await.expect("where d1");
    let n1 = names(&w1);
    assert!(n1.contains("Alice"), "WHERE form must reach the 1-hop Alice; got {n1:?}");
    assert!(
        !n1.contains("Lunaris"),
        "WHERE form depth=1 must NOT reach the 2-hop Lunaris (root+depth constrain); got {n1:?}"
    );

    // FIXED form depth=2 → Alice + Lunaris.
    let w2 = storage.graph_traverse(&scope, &q(anchored(2, false)), None).await.expect("where d2");
    let n2 = names(&w2);
    assert!(
        n2.contains("Alice") && n2.contains("Lunaris"),
        "WHERE form depth=2 reaches both 1- and 2-hop nodes; got {n2:?}"
    );

    // FIXED (inline) form, root=Bob depth=1 → ONLY Alice. Pre-v3-2 Moon
    // ignored the inline-property anchor filter and this wrongly reached the
    // 2-hop Lunaris; PR #193 fixed it, and production point-lookups now use
    // the inline form for IndexScan narrowing — so inline must behave
    // identically to the WHERE form here.
    let i1 = storage.graph_traverse(&scope, &q(anchored(1, true)), None).await.expect("inline d1");
    let ni1 = names(&i1);
    assert!(ni1.contains("Alice"), "inline form must reach the 1-hop Alice; got {ni1:?}");
    assert!(
        !ni1.contains("Lunaris"),
        "inline-property filter must constrain on Moon ≥ v0.4.0 → depth=1 must NOT reach the \
         2-hop Lunaris (pre-v3-2 regression guard); got {ni1:?}"
    );

    // Inline depth=2 parity with the WHERE form.
    let i2 = storage.graph_traverse(&scope, &q(anchored(2, true)), None).await.expect("inline d2");
    let ni2 = names(&i2);
    assert!(
        ni2.contains("Alice") && ni2.contains("Lunaris"),
        "inline form depth=2 reaches both 1- and 2-hop nodes; got {ni2:?}"
    );
}
