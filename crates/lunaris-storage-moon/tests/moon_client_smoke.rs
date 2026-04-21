//! Phase 1.5 retrofit (STORE-09) smoke test — proves that after swapping the
//! hand-rolled `redis 0.32+` RESP wrappers for the typed `moon-client` v0.1.x SDK,
//! the `MoonStorage` round-trip and capability surface still hold.
//!
//! Gated behind the `moon-it` Cargo feature so CI without a reachable Moon instance
//! does NOT fail on the per-commit gate. To run:
//!
//! ```bash
//! cargo test -p lunaris-storage-moon --features moon-it --test moon_client_smoke
//! ```
//!
//! Expects Moon at `MOON_URL` env var (default `moon://localhost:6390`).
//!
//! ## What this proves
//!
//! 1. `MoonStorage::connect(url)` now uses `moon-client::MoonClient::connect(...)`
//!    under the hood and produces an Episode round-trip identical to Phase 1's
//!    `episode_roundtrip.rs`.
//! 2. `MoonStorage::capabilities().native_rrf == true` after the Gap 9
//!    closure. `ensure_indexes` now declares `SchemaField::Text("content")`
//!    on every Lunaris FT index and `WriteOp::VectorUpsert` writes the
//!    per-row text payload via `extract_content_for_index` (mirrors the
//!    Postgres tsvector convention), so Moon's HYBRID FT.SEARCH resolves
//!    `@content` and `fuse_rrf` opts into `RrfFusion::Moon` for one-RTT
//!    server-side fusion.
//!
//! ## What this does NOT prove
//!
//! End-to-end `text().hybrid_search()` against live indexes — that is exercised in
//! Phase 2 plan 02-02 (Keyword BM25 + RRF) once the BM25 + vector indexes exist.
//! Here we only assert the `native_rrf` capability bit so the planner has the
//! signal it needs.

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, StoragePort, WriteOp};
use lunaris_storage_moon::{MoonStorage, keyspace};

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6390".into())
}

#[tokio::test]
async fn round_trip_via_moon_client() {
    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6390");

    let clock = HlcClock::new(0);
    let ep = Episode::new("smoke://retrofit", "hello moon-client", &clock);
    let key = keyspace::episode_key(ep.id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    let lsn = storage
        .atomic_write(&[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write commit via moon-client");
    assert!(
        lsn.wall_ms > 0 || lsn.counter > 0,
        "Lsn must be non-zero after the retrofit, got {lsn:?}"
    );

    let now = clock.tick();
    let row = storage
        .read_as_of(&key, now)
        .await
        .expect("read_as_of ok via moon-client")
        .expect("episode exists");
    assert_eq!(row.key, key, "key roundtrip via moon-client");
    assert_eq!(
        row.value,
        Bytes::from(value),
        "value roundtrip — bytes must be identical via moon-client"
    );
}

#[tokio::test]
async fn capabilities_reports_native_rrf() {
    let storage = MoonStorage::connect(&moon_url()).await.expect("connect to Moon");
    let cap = storage.capabilities();
    assert!(
        cap.native_rrf,
        "Moon backend reports native_rrf=true — ensure_indexes declares a \
         SchemaField::Text(\"content\") per index and VectorUpsert writes the \
         per-row text payload via extract_content_for_index; HYBRID FT.SEARCH \
         resolves @content and fuse_rrf opts into RrfFusion::Moon (Gap 9 \
         closure 2026-04-21)."
    );
    // Also re-assert the rest of the Moon profile so any drift surfaces here.
    assert!(
        !cap.bi_temporal_native,
        "Moon does not natively support KV bi-temporal reads (HGET ignores AS_OF) — \
         only FT.SEARCH AS_OF + GRAPH.QUERY VALID_AT are temporal (Gap 8)"
    );
    assert!(cap.graph_native);
    assert!(cap.rerank_native);
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768);
}

/// Gap 9 follow-on guard — proves that `native_rrf=true` is more than a
/// claim: a real `text().hybrid_search()` round-trip against a freshly-seeded
/// row resolves `@content` and returns the row instead of "unknown index".
///
/// Without this assertion, the bit and the behavior can drift again — it
/// took the live recall_q probe to catch the previous regression.
#[tokio::test]
async fn hybrid_search_round_trip_after_ensure_indexes() {
    use lunaris_core::WriteOp;

    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6390");

    // Seed one fact with a distinctive predicate so BM25 can find it.
    let id = ulid::Ulid::new().to_bytes().to_vec();
    let predicate = "moon-it-smoke-hybrid-marker";
    let fact_text = format!("subject-id-stub {predicate}");
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) * 0.001).collect();
    storage
        .atomic_write(&[WriteOp::VectorUpsert {
            index: "facts".into(),
            id: id.clone(),
            embedding: embedding.clone(),
            metadata: serde_json::json!({
                "predicate": predicate,
                "subject": "subject-id-stub",
                "fact_text": fact_text,
            }),
        }])
        .await
        .expect("seed VectorUpsert via Gap-9 content schema");

    // Drive the same call path the recall hot loop uses: native HYBRID via
    // the moon SDK, with both BM25 and dense weights non-zero.
    let mut typed = storage.client().typed();
    let mut text = typed.text();
    let weights: [f64; 3] = [0.5, 0.5, 0.0];
    let hits = text
        .hybrid_search("facts", predicate, &embedding, "vec", None, 5, weights)
        .await
        .expect("hybrid_search must succeed once @content is in the schema (Gap 9 closure)");

    assert!(
        hits.iter().any(|h| h.key.as_bytes().ends_with(&id) || h.key.contains(&hex::encode(&id))),
        "seeded fact must appear in HYBRID hits — got {} hits, none matching marker",
        hits.len()
    );
}
