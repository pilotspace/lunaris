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
//! RFC 0001 Wave 0: StoragePort methods now take `&Scope`. Tests pass `&Scope::dev()`.

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, Scope, StoragePort, WriteOp};
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
    let ep = Episode::new(Scope::dev(), "smoke://retrofit", "hello moon-client", &clock);
    let key = keyspace::episode_key(&Scope::dev(), ep.id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    let lsn = storage
        .atomic_write(&Scope::dev(), &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write commit via moon-client");
    assert!(
        lsn.wall_ms > 0 || lsn.counter > 0,
        "Lsn must be non-zero after the retrofit, got {lsn:?}"
    );

    let now = clock.tick();
    let row = storage
        .read_as_of(&Scope::dev(), &key, now)
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
    assert!(
        !cap.bi_temporal_native,
        "Moon does not natively support KV bi-temporal reads (HGET ignores AS_OF)"
    );
    assert!(cap.graph_native);
    assert!(cap.rerank_native);
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768);
}

#[tokio::test]
async fn hybrid_search_round_trip_after_ensure_indexes() {
    use lunaris_core::WriteOp;

    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6390");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let predicate = "moon-it-smoke-hybrid-marker";
    let fact_text = format!("subject-id-stub {predicate}");
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) * 0.001).collect();
    storage
        .atomic_write(
            &Scope::dev(),
            &[WriteOp::VectorUpsert {
                index: "facts".into(),
                id: id.clone(),
                embedding: embedding.clone(),
                metadata: serde_json::json!({
                    "predicate": predicate,
                    "subject": "subject-id-stub",
                    "fact_text": fact_text,
                }),
            }],
        )
        .await
        .expect("seed VectorUpsert via Gap-9 content schema");

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
