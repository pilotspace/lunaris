//! Reverse-ratchet — fire when Moon's Cypher WRITE executor learns to use the
//! property index.
//!
//! ## The defect
//!
//! Creating a graph node takes two round trips: `GRAPH.ADDNODE` registers the
//! node and its `_key`, then a `MATCH … SET` writes `id` and the caller's
//! props onto it (`atomic.rs`, the `WriteOp::GraphNode` arm). That second
//! statement is a WRITE, and Moon's write executor does not narrow the `MATCH`
//! through the per-segment property index — it visits every node carrying the
//! label. So creating a node costs O(nodes already present), and building a
//! graph of N nodes costs O(N^2).
//!
//! Measured live: a 50-node batch cost 20 ms at 1.2k nodes and 139 ms at 10k,
//! while the same batch of `VectorUpsert`, `GraphEdge` and `KvPut` ops stayed
//! flat. That is the whole of the INGEST-06 graph-on budget miss — p50 863 ms
//! against a 300 ms budget on CI, 869 ms on macOS, from a payload that is a
//! constant 5 chunks / 50 entities / 150 relations every time.
//!
//! ## Why it is the executor and not the query shape
//!
//! This was very nearly "fixed" by rewriting the statement to give the planner
//! an inline property map, because `GRAPH.PROFILE` on a 3,010-node graph says
//! that shape is indexed:
//!
//! | shape                                       | plan        | rows visited |
//! |---------------------------------------------|-------------|--------------|
//! | `MATCH (n:L) WHERE id(n) = k`               | `NodeScan`  | 3010         |
//! | `MATCH (n:L {_key: '..'})`                  | `IndexScan` | 1            |
//! | `MATCH (n:L {_key: '..'}) WHERE id(n) = k`  | `IndexScan` | 1            |
//!
//! That rewrite changed nothing, because `GRAPH.PROFILE` refuses writes, so
//! every row above is a READ plan. The discriminator is below and it is the
//! reason this file exists: a byte-identical predicate is flat as a `RETURN`
//! and grows as a `SET`. The shape was never the variable.
//!
//! ## Why a reverse-ratchet rather than a park
//!
//! `lunaris-storage-moon`'s integration job runs the package with
//! `--include-ignored`, so an `#[ignore]` here would not be parked — it would
//! just be red. And the real hazard is the opposite one: once the INGEST-06
//! budget is written off as "known upstream issue", nothing would ever say
//! that it had been fixed. So this asserts the defect is STILL PRESENT. It
//! passes today and fails the moment the vendored Moon stops scanning, at
//! which point the failure message is the to-do list.
//!
//! Filed upstream as pilotspace/moon#719.

#![cfg(feature = "moon-it")]

use std::time::Instant;

use lunaris_test_harness::EphemeralMoon;

/// Nodes per round. Batched so a round is well above clock noise.
const PER_ROUND: usize = 50;
/// 200 rounds = 10,000 nodes. Fewer rounds shrink the signal: at 3k nodes the
/// same defect only showed ~1.6x, which no honest bound separates from noise.
const ROUNDS: usize = 200;
/// Rounds 0..WARMUP are discarded — the first writes pay allocation and index
/// creation costs unrelated to graph size.
const WARMUP: usize = 20;
const WINDOW: usize = 10;

/// The write must still be growing by at least this much. Measured 6.3x-9.1x
/// across runs, so this has ~2.5x of headroom before it would flake.
const WRITE_GROWTH_FLOOR: f64 = 2.5;
/// The read control must stay flat. If it does not, the machine was too noisy
/// for the write measurement to mean anything either, and this test says that
/// rather than reporting a verdict it cannot support.
const READ_GROWTH_CEILING: f64 = 2.0;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
    v[v.len() / 2]
}

fn growth(samples: &[f64]) -> f64 {
    let early = median(samples[WARMUP..WARMUP + WINDOW].to_vec());
    let late = median(samples[samples.len() - WINDOW..].to_vec());
    late / early
}

#[tokio::test(flavor = "multi_thread")]
async fn moon_cypher_writes_still_scan_every_node_of_the_label() {
    let moon = EphemeralMoon::spawn().await.expect("ephemeral Moon for the scan measurement");
    let client =
        redis::Client::open(moon.url().replace("moon://", "redis://")).expect("redis client");
    let mut con = client.get_multiplexed_async_connection().await.expect("connection");
    let graph = "reverse_ratchet_graph";

    // Moon does not auto-create graphs; ADDNODE against a missing one errors.
    let _: redis::Value =
        redis::cmd("GRAPH.CREATE").arg(graph).query_async(&mut con).await.expect("create graph");

    let mut write_ms = Vec::with_capacity(ROUNDS);
    let mut read_ms = Vec::with_capacity(ROUNDS);

    for r in 0..ROUNDS {
        let (mut w, mut rd) = (0.0, 0.0);
        for k in 0..PER_ROUND {
            let i = r * PER_ROUND + k;
            let hex = format!("{i:032x}");
            let key = format!("idx:{hex}");

            let node_id: i64 = redis::cmd("GRAPH.ADDNODE")
                .arg(graph)
                .arg("Probe")
                .arg("_key")
                .arg(&key)
                .query_async(&mut con)
                .await
                .expect("addnode");

            // The production create-path statement.
            let t = Instant::now();
            let _: redis::Value = redis::cmd("GRAPH.QUERY")
                .arg(graph)
                .arg(format!(
                    "MATCH (n:Probe {{_key: '{key}'}}) WHERE id(n) = {node_id} \
                     SET n.id = '{hex}' RETURN n"
                ))
                .query_async(&mut con)
                .await
                .expect("create-path SET");
            w += t.elapsed().as_secs_f64() * 1000.0;

            // CONTROL: byte-identical predicate, `RETURN` instead of `SET`.
            // Same graph, same round, same connection — the only variable
            // left is whether the statement writes.
            let t = Instant::now();
            let _: redis::Value = redis::cmd("GRAPH.QUERY")
                .arg(graph)
                .arg(format!("MATCH (n:Probe {{_key: '{key}'}}) WHERE id(n) = {node_id} RETURN n"))
                .query_async(&mut con)
                .await
                .expect("control read");
            rd += t.elapsed().as_secs_f64() * 1000.0;
        }
        write_ms.push(w);
        read_ms.push(rd);
    }

    let write_growth = growth(&write_ms);
    let read_growth = growth(&read_ms);

    // Printed on pass too: a later reader needs the baseline to judge drift.
    eprintln!(
        "graph write vs read over {} nodes — write growth {write_growth:.2}x \
         (floor {WRITE_GROWTH_FLOOR:.1}x), read control {read_growth:.2}x \
         (ceiling {READ_GROWTH_CEILING:.1}x)",
        ROUNDS * PER_ROUND,
    );

    assert!(
        read_growth <= READ_GROWTH_CEILING,
        "the READ control grew {read_growth:.2}x (ceiling {READ_GROWTH_CEILING:.1}x). The read \
         path is indexed and must stay flat, so this run measured something other than the \
         defect — a loaded machine, a noisy disk, or a change to the read planner. The write \
         number from this run ({write_growth:.2}x) cannot be trusted either way."
    );

    assert!(
        write_growth >= WRITE_GROWTH_FLOOR,
        "Cypher WRITES no longer scale with graph size: write growth {write_growth:.2}x is below \
         the {WRITE_GROWTH_FLOOR:.1}x floor, while the read control is flat at \
         {read_growth:.2}x. That is what an upstream fix looks like (pilotspace/moon#719). \
         To-do: (1) re-measure the INGEST-06 graph-on bench — it was p50 863 ms against a \
         300 ms budget purely because of this; (2) drop the graph-on row's known-miss note in \
         docs/planning/2026-08-21-ship-plan.md; (3) replace this reverse-ratchet with a \
         forward assertion that node-create cost stays FLAT."
    );
}
