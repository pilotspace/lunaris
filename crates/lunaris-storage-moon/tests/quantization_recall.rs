//! ADD task `sq8-quantization-optin` — live recall@10 eval at 768-d
//! (contract FROZEN @ v1, 2026-06-11; **default flipped to SQ8 by
//! moon-v051-perf-exploit W1-1, 2026-07-10** — see
//! `docs/design/moon-v051-perf-exploit.md` §W1 and
//! `lunaris_storage_moon::client::resolve_quantization`). Gated behind
//! `moon-it` + `MOON_URL`.
//!
//! Both index sets are provisioned through the PRODUCTION per-scope path
//! (`MoonStorage` handle → `ensure_scope` → FT.CREATE). Since W1-1, a PLAIN
//! handle (no `?quant=`) now defaults to SQ8, so the TQ4 baseline here is
//! requested explicitly via `?quant=tq4` (the RSS-constrained escape
//! hatch) and the SQ8 candidate via `?quant=sq8` (now redundant with the
//! default, kept explicit so this eval stays meaningful even if the
//! default changes again). Vectors are written through `atomic_write` and
//! compaction is forced with `FT.COMPACT` so recall is measured over
//! quantized HNSW segments, not the raw write buffer.

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_moon::client::Quantization;
use serde_json::json;
use ulid::Ulid;

mod common;
const DIM: usize = 768;
const N_DOCS: usize = 600;
const N_QUERIES: usize = 20;
const K: usize = 10;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

fn url_with(params: &str) -> String {
    let base = url();
    if base.contains('?') { format!("{base}&{params}") } else { format!("{base}?{params}") }
}

async fn connect_or_skip(u: &str) -> Option<MoonStorage> {
    match MoonStorage::connect(u).await {
        Ok(s) => Some(s),
        Err(e) => {
            common::note_moon_unreachable(e);
            None
        }
    }
}

fn fresh_scope(tag: &str) -> Scope {
    Scope::new(format!("quant-{tag}-{}", Ulid::new().to_string().to_lowercase()))
        .expect("valid scope")
}

/// Deterministic pseudo-Gaussian-ish vector from a seed (xorshift64*; no
/// rand dependency). Box-Muller is overkill — uniform components are fine
/// for a comparative recall eval as long as ground truth uses the same data.
fn det_vec(seed: u64) -> Vec<f32> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(DIM);
    for _ in 0..DIM {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Map to [-1, 1).
        v.push(((x >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0);
    }
    v
}

fn doc_id(i: usize) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&(i as u64).to_be_bytes());
    id[15] = b'Q';
    id
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt()).max(1e-12)
}

/// Brute-force top-K doc indices for a query over the corpus.
fn ground_truth(q: &[f32], corpus: &[Vec<f32>]) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> =
        corpus.iter().enumerate().map(|(i, v)| (i, cosine(q, v))).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(K).map(|(i, _)| i).collect()
}

/// Seed N_DOCS vectors into the scope's `chunks` index through atomic_write,
/// then force compaction on the per-scope chunks FT index.
async fn seed_corpus(moon: &MoonStorage, scope: &Scope, corpus: &[Vec<f32>]) {
    // Batch in chunks of 100 ops to keep TXNs reasonable.
    for batch in corpus.chunks(100).enumerate().map(|(bi, ch)| (bi * 100, ch)) {
        let (base, ch) = batch;
        let ops: Vec<WriteOp> = ch
            .iter()
            .enumerate()
            .map(|(j, v)| WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: doc_id(base + j),
                embedding: v.clone(),
                metadata: json!({"text": format!("doc {}", base + j), "source": "quant-eval"}),
            })
            .collect();
        moon.atomic_write(scope, &ops).await.expect("corpus write");
    }
    // Force compaction so vectors live in quantized HNSW segments.
    let mut typed = moon.client().typed();
    let conn = typed.inner_mut();
    let idx = format!("lunaris_{}_chunks_idx", scope.as_str());
    let _: redis::Value =
        redis::cmd("FT.COMPACT").arg(&idx).query_async(conn).await.expect("FT.COMPACT");
}

/// recall@10 averaged over N_QUERIES held-out probe vectors.
async fn measure_recall(moon: &MoonStorage, scope: &Scope, corpus: &[Vec<f32>]) -> f64 {
    let mut total = 0.0f64;
    for qi in 0..N_QUERIES {
        // Query = perturbed corpus vector so ground truth is non-trivial.
        let base = &corpus[qi * 7 % N_DOCS];
        let noise = det_vec(0xABCD_0000 + qi as u64);
        let q: Vec<f32> = base.iter().zip(&noise).map(|(b, n)| b + 0.05 * n).collect();

        let truth = ground_truth(&q, corpus);
        let hits = moon
            .vector_search(scope, "chunks", &q, K, None, None, false)
            .await
            .expect("vector search");
        let got: Vec<usize> = hits
            .iter()
            .filter_map(|h| {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(h.id.get(..8)?);
                Some(u64::from_be_bytes(buf) as usize)
            })
            .collect();
        let overlap = truth.iter().filter(|t| got.contains(t)).count();
        total += overlap as f64 / K as f64;
    }
    total / N_QUERIES as f64
}

/// §2 DISCRIMINATOR + eval — SQ8 recall@10 must clear the 0.90 floor AND
/// match-or-beat the TQ4 escape-hatch tier at 768-d.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sq8_beats_or_matches_tq4_recall_at_768d() {
    let Some(tq4) = connect_or_skip(&url_with("quant=tq4")).await else { return };
    let Some(sq8) = connect_or_skip(&url_with("quant=sq8")).await else { return };
    assert_eq!(sq8.client().quantization, Some(Quantization::Sq8), "handle records the choice");
    assert_eq!(tq4.client().quantization, Some(Quantization::Tq4), "explicit tq4 override honored");

    let corpus: Vec<Vec<f32>> = (0..N_DOCS).map(|i| det_vec(i as u64 + 1)).collect();

    let scope_tq4 = fresh_scope("tq4");
    let scope_sq8 = fresh_scope("sq8");
    seed_corpus(&tq4, &scope_tq4, &corpus).await;
    seed_corpus(&sq8, &scope_sq8, &corpus).await;

    let recall_tq4 = measure_recall(&tq4, &scope_tq4, &corpus).await;
    let recall_sq8 = measure_recall(&sq8, &scope_sq8, &corpus).await;
    eprintln!(
        "QUANT EVAL VERDICT: sq8={recall_sq8:.3} tq4={recall_tq4:.3} (768-d, n={N_DOCS}, q={N_QUERIES}, k={K})"
    );

    assert!(recall_sq8 >= 0.90, "SQ8 recall@10 must clear the 0.90 floor, got {recall_sq8:.3}");
    assert!(
        recall_sq8 >= recall_tq4,
        "SQ8 must match-or-beat the TQ4 default: sq8={recall_sq8:.3} tq4={recall_tq4:.3}"
    );
}

/// p50/p99 `vector_search` latency, sequential, over `N_QUERIES` queries
/// against an already-compacted index.
async fn measure_latency_ms(moon: &MoonStorage, scope: &Scope, corpus: &[Vec<f32>]) -> (f64, f64) {
    let mut samples = Vec::with_capacity(N_QUERIES);
    for qi in 0..N_QUERIES {
        let base = &corpus[qi * 7 % N_DOCS];
        let noise = det_vec(0xABCD_0000 + qi as u64);
        let q: Vec<f32> = base.iter().zip(&noise).map(|(b, n)| b + 0.05 * n).collect();
        let start = std::time::Instant::now();
        moon.vector_search(scope, "chunks", &q, K, None, None, false).await.expect("vector search");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = samples[samples.len() / 2];
    let p99 = samples[(samples.len() * 99 / 100).min(samples.len() - 1)];
    (p50, p99)
}

/// moon-v051-perf-exploit W1 exit criterion — "SQ8+ef default beats TQ4
/// baseline on recall@10 at lower or equal p50". Reuses the recall test's
/// corpus/seed helpers against fresh scopes, on the SAME already-compacted
/// indexes, then times `vector_search` sequentially.
///
/// The latency assertion is intentionally generous (2×, not strict ≤) —
/// this runs on a shared dev host (other moon-v051-perf-exploit agents'
/// Moon instances are also live on this machine) where wall-clock p50/p99
/// carries real scheduling noise; the recall assertion above is the hard
/// gate, this one is meant to catch a gross regression (e.g. SQ8 somehow
/// coming out 5× slower) while reporting the real numbers either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sq8_p50_latency_within_tolerance_of_tq4() {
    let Some(tq4) = connect_or_skip(&url_with("quant=tq4")).await else { return };
    let Some(sq8) = connect_or_skip(&url_with("quant=sq8")).await else { return };

    let corpus: Vec<Vec<f32>> = (0..N_DOCS).map(|i| det_vec(i as u64 + 1)).collect();
    let scope_tq4 = fresh_scope("tq4-lat");
    let scope_sq8 = fresh_scope("sq8-lat");
    seed_corpus(&tq4, &scope_tq4, &corpus).await;
    seed_corpus(&sq8, &scope_sq8, &corpus).await;

    // Warm up (first query pays connection/cache warmup cost).
    let _ = tq4.vector_search(&scope_tq4, "chunks", &corpus[0], K, None, None, false).await;
    let _ = sq8.vector_search(&scope_sq8, "chunks", &corpus[0], K, None, None, false).await;

    let (tq4_p50, tq4_p99) = measure_latency_ms(&tq4, &scope_tq4, &corpus).await;
    let (sq8_p50, sq8_p99) = measure_latency_ms(&sq8, &scope_sq8, &corpus).await;
    eprintln!(
        "QUANT LATENCY VERDICT: sq8 p50={sq8_p50:.3}ms p99={sq8_p99:.3}ms | \
         tq4 p50={tq4_p50:.3}ms p99={tq4_p99:.3}ms (768-d, n={N_DOCS}, q={N_QUERIES}, k={K})"
    );

    assert!(
        sq8_p50 <= tq4_p50 * 2.0 + 1.0,
        "SQ8 p50 must stay within a generous 2x+1ms tolerance of TQ4 p50 (gross-regression \
         guard, not a precise bound on a shared dev host): sq8={sq8_p50:.3}ms tq4={tq4_p50:.3}ms"
    );
}

/// §2 — a `?quant=sq8` handle provisions per-scope indices that round-trip
/// vectors through the production write/read path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quant_handle_provisions_scoped_indexes() {
    let Some(sq8) = connect_or_skip(&url_with("quant=sq8")).await else { return };
    let scope = fresh_scope("prov");
    let v = det_vec(99);
    let ops = vec![WriteOp::VectorUpsert {
        index: "entities".into(),
        id: doc_id(1),
        embedding: v.clone(),
        metadata: json!({"entity_type": "Person", "name": "Q"}),
    }];
    sq8.atomic_write(&scope, &ops).await.expect("write through quantized scope index");
    let hits =
        sq8.vector_search(&scope, "entities", &v, 1, None, None, false).await.expect("search");
    assert_eq!(hits.len(), 1, "self-query must return the written vector");
    assert_eq!(hits[0].id, doc_id(1));
}

/// §2 — ws and quant coexist in either order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_and_quant_params_coexist() {
    let Some(a) = connect_or_skip(&url_with("ws=hot&quant=sq8")).await else { return };
    assert_eq!(a.client().workspace.as_deref(), Some("hot"));
    assert_eq!(a.client().quantization, Some(Quantization::Sq8));
    let Some(b) = connect_or_skip(&url_with("quant=tq4&ws=hot")).await else { return };
    assert_eq!(b.client().workspace.as_deref(), Some("hot"));
    assert_eq!(b.client().quantization, Some(Quantization::Tq4));
}

/// moon-v051-perf-exploit W1-1 DISCRIMINATOR — a PLAIN handle (no `?quant=`
/// at all) must default to SQ8 through the real `MoonStorage::connect`
/// path, not just the pure `resolve_quantization` unit test in client.rs.
/// This is the live proof that the default actually reaches the network
/// connect flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_handle_defaults_to_sq8() {
    let Some(plain) = connect_or_skip(&url()).await else { return };
    assert_eq!(
        plain.client().quantization,
        Some(Quantization::Sq8),
        "v0.5.1+ default: an absent ?quant= now resolves to Sq8, not the old TQ4-via-None path"
    );
}
