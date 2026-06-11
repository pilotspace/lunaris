//! ADD task `bench-rerun-v030` — sized plain-vector vs Navigate recall A/B
//! (contract FROZEN @ v1, 2026-06-11). Gated behind `moon-it` + `MOON_URL`.
//!
//! Scaled-up version of the navigate_recall.rs discriminator: a corpus of
//! background docs plus M "target" docs that are VECTOR-FAR from their query
//! but GRAPH-LINKED (2 hops) to a near anchor — the recall edge Navigate is
//! claimed to provide on graph-linked corpora (and ONLY there; the docs keep
//! that qualifier). Asserts nav recall@5 >= plain recall@5 and prints the
//! verdict line consumed by docs/benchmarks/v0.7-moon-v030-rerun.md.

#![cfg(feature = "moon-it")]

use std::time::Instant;

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{NavigateSpec, WriteOp};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

const DIM: usize = 768;
/// Background corpus size (isolated vector-mid distractors).
const N_BACKGROUND: usize = 500;
/// Query clusters; each contributes one anchor + one graph-linked far target.
const N_CLUSTERS: usize = 25;
const K: usize = 5;

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

/// Deterministic 16-byte id: cluster index + role marker.
fn cid(cluster: usize, role: u8) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&(cluster as u64).to_be_bytes());
    id[15] = role;
    id
}

/// xorshift64* pseudo-random unit-ish vector (same generator as the
/// quantization eval — deterministic, no rand dependency).
fn det_vec(seed: u64) -> Vec<f32> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(DIM);
    for _ in 0..DIM {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push(((x >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0);
    }
    v
}

/// Cluster anchor direction: deterministic base vector per cluster.
fn anchor_vec(cluster: usize) -> Vec<f32> {
    det_vec(0xA000 + cluster as u64)
}

/// The far target: orthogonal-ish direction (different seed) so plain KNN at
/// k=5 cannot reach it from the cluster query.
fn target_vec(cluster: usize) -> Vec<f32> {
    det_vec(0xF000 + cluster as u64)
}

fn node(graph_id: Vec<u8>, name: String) -> WriteOp {
    WriteOp::GraphNode {
        graph: "lunaris_graph".into(),
        id: graph_id.clone(),
        label: "Person".into(),
        props: json!({
            "id_hex": hex::encode(&graph_id),
            "name": name,
            "type": "Person",
            "confidence": 0.9,
        }),
    }
}

fn vector(id: Vec<u8>, e: Vec<f32>) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "entities".into(),
        id: id.clone(),
        embedding: e,
        metadata: json!({"entity_type": "Person", "name": hex::encode(id)}),
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

/// Seed everything through the production atomic_write path, batched.
async fn seed(moon: &MoonStorage, scope: &Scope) {
    // Background distractors: vector-only, no graph nodes.
    let mut ops: Vec<WriteOp> = Vec::new();
    for i in 0..N_BACKGROUND {
        ops.push(vector(cid(1000 + i, b'x'), det_vec(0xB000 + i as u64)));
        if ops.len() >= 100 {
            moon.atomic_write(scope, &ops).await.expect("background write");
            ops.clear();
        }
    }
    if !ops.is_empty() {
        moon.atomic_write(scope, &ops).await.expect("background write");
        ops.clear();
    }

    // Clusters: anchor A_i (near query), bridge B_i, far target T_i with
    // graph chain A_i -> B_i -> T_i. Bridge gets a mid vector so it's real.
    for c in 0..N_CLUSTERS {
        let a = cid(c, b'a');
        let b = cid(c, b'b');
        let t = cid(c, b't');
        let ops = vec![
            node(a.clone(), format!("anchor-{c}")),
            vector(a.clone(), anchor_vec(c)),
            node(b.clone(), format!("bridge-{c}")),
            vector(b.clone(), det_vec(0xC000 + c as u64)),
            node(t.clone(), format!("target-{c}")),
            vector(t.clone(), target_vec(c)),
            edge(a.clone(), b.clone()),
            edge(b, t),
        ];
        moon.atomic_write(scope, &ops).await.expect("cluster write");
    }
}

/// Per-cluster query: a small perturbation of the anchor direction.
fn query_vec(cluster: usize) -> Vec<f32> {
    let a = anchor_vec(cluster);
    let n = det_vec(0xD000 + cluster as u64);
    a.iter().zip(&n).map(|(x, y)| x + 0.05 * y).collect()
}

fn pct50(xs: &mut [f64]) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs[xs.len() / 2]
}

/// §2 — Navigate recall@5 must match-or-beat plain vector recall@5 on the
/// graph-linked targets, with per-query p50s printed for the benchmarks doc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigate_beats_plain_on_graph_linked_corpus() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope =
        Scope::new(format!("navab-{}", Ulid::new().to_string().to_lowercase())).expect("scope");
    seed(&moon, &scope).await;

    let spec = NavigateSpec::new(2).expect("valid hops");
    let mut plain_hits_targets = 0usize;
    let mut nav_hits_targets = 0usize;
    let mut plain_lat_ms: Vec<f64> = Vec::with_capacity(N_CLUSTERS);
    let mut nav_lat_ms: Vec<f64> = Vec::with_capacity(N_CLUSTERS);

    for c in 0..N_CLUSTERS {
        let q = query_vec(c);
        let target = cid(c, b't');

        let t0 = Instant::now();
        let plain =
            moon.vector_search(&scope, "entities", &q, K, None, None, false).await.expect("plain");
        plain_lat_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        if plain.iter().any(|h| h.id == target) {
            plain_hits_targets += 1;
        }

        let t1 = Instant::now();
        let nav =
            moon.vector_navigate(&scope, "entities", &q, K, &spec).await.expect("navigate");
        nav_lat_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
        if nav.iter().any(|h| h.id == target) {
            nav_hits_targets += 1;
        }
    }

    let plain_recall = plain_hits_targets as f64 / N_CLUSTERS as f64;
    let nav_recall = nav_hits_targets as f64 / N_CLUSTERS as f64;
    let plain_p50 = pct50(&mut plain_lat_ms);
    let nav_p50 = pct50(&mut nav_lat_ms);
    eprintln!(
        "NAV A/B VERDICT: plain={plain_recall:.2} nav={nav_recall:.2} \
         plain_p50={plain_p50:.2}ms nav_p50={nav_p50:.2}ms \
         (graph-linked targets, n={N_BACKGROUND}+{N_CLUSTERS}x3, k={K}, hops=2, 768-d)"
    );

    assert!(
        nav_recall >= plain_recall,
        "Navigate must match-or-beat plain vector recall on a graph-linked corpus: \
         nav={nav_recall:.2} plain={plain_recall:.2} — this would falsify the \
         ft-navigate-recall claim (change request, not a test edit)"
    );
}
