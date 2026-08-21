//! Phase 1.5 retrofit (STORE-09) smoke test — proves that after swapping the
//! hand-rolled `redis 0.32+` RESP wrappers for the typed `moon-client` v0.1.x SDK,
//! the `MoonStorage` round-trip and capability surface still hold.
//!
//! Gated behind the `moon-it` Cargo feature so CI without a reachable Moon instance
//! does NOT fail on the per-commit gate. To run:
//!
//! ```bash
//! MOON_URL=moon://127.0.0.1:6380 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test moon_client_smoke
//! ```
//!
//! Dials `MOON_URL` (default `moon://localhost:6380`) and PANICS on connect if
//! nothing is listening.
//!
//! ## One partition per test (F11)
//!
//! Every test in this file used to write under `Scope::dev()`, which put all of
//! them in ONE Moon FT index (`ft_index_name(scope, kind)`) and one graph
//! (`graph_key(scope)`). `vector_search_as_of_round_trip_via_moon` asks for the
//! top 5 and asserts its own seeded id is among them — and since the seeded
//! embeddings are near-identical ramps that all score 1.0, which 5 come back is
//! arbitrary once the index holds more than five. Measured: green, green, then
//! RED on the third consecutive run against the same Moon, and RED under the
//! parallel `--include-ignored` sweep on a many-core box while staying green on
//! CI's smaller runner. A top-k assertion over a shared, growing index is a
//! flake with a timer on it.
//!
//! So each test takes its own `test_scope(...)`. The scope is what Moon
//! namespaces the vector and graph legs by, so an isolated partition means the
//! top-5 can only ever contain this test's own rows.

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::storage::keyword::KeywordPort;
use lunaris_core::{CypherQuery, Episode, HlcClock, Scope, StoragePort, WriteOp};
use lunaris_storage_moon::{MoonStorage, keyspace};
use std::time::Duration;

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".into())
}

/// A partition no other test — or earlier run of this one — is using.
///
/// `name` is only there to make a stuck key readable in `redis-cli`; the ULID
/// is what makes it unique. Crockford's alphabet is a subset of the scope
/// alphabet `[A-Za-z0-9_\-.]{1,128}`, so `Scope::new` cannot reject this.
fn test_scope(name: &str) -> Scope {
    Scope::new(format!("moonit-{name}-{}", ulid::Ulid::new()))
        .expect("ULID-suffixed label is inside the scope alphabet")
}

#[tokio::test]
async fn round_trip_via_moon_client() {
    let scope = test_scope("kv-roundtrip");
    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6380");

    let clock = HlcClock::new(0);
    let ep = Episode::new(scope.clone(), "smoke://retrofit", "hello moon-client", &clock);
    let key = keyspace::episode_key(&scope, ep.id);
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    let lsn = storage
        .atomic_write(&scope, &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write commit via moon-client");
    assert!(
        lsn.wall_ms > 0 || lsn.counter > 0,
        "Lsn must be non-zero after the retrofit, got {lsn:?}"
    );

    let now = clock.tick();
    let row = storage
        .read_as_of(&scope, &key, now)
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
async fn queue_publish_subscribe_round_trip_via_moon_mq() {
    let scope = test_scope("queue");
    let storage = MoonStorage::connect(&moon_url()).await.expect("connect to Moon");
    assert!(storage.capabilities().queue_native);

    let topic = format!("moon-it-queue-{}", ulid::Ulid::new());
    let payload = Bytes::from_static(b"moon-mq-round-trip");
    let mut stream = storage
        .subscribe(&scope, "moon-it-consumer", &topic, 0)
        .await
        .expect("subscribe to Moon MQ");

    let offset =
        storage.publish(&scope, &topic, 0, payload.clone()).await.expect("publish to Moon MQ");
    assert!(offset > 0, "Moon MQ stream id should map to a non-zero offset");

    let msg = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("Moon MQ payload should arrive within timeout")
        .expect("Moon MQ stream should yield")
        .expect("Moon MQ message should be ok");
    assert_eq!(msg.payload, payload);
    assert_eq!(msg.partition, 0);
}

#[tokio::test]
async fn hybrid_search_round_trip_after_ensure_indexes() {
    let scope = test_scope("hybrid");
    use lunaris_core::WriteOp;

    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6380");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let predicate = "moon-it-smoke-hybrid-marker";
    let fact_text = format!("subject-id-stub {predicate}");
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) * 0.001).collect();
    storage
        .atomic_write(
            &scope,
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

    let typed = storage.client().typed();
    let mut text = typed.text();
    let weights: [f64; 3] = [0.5, 0.5, 0.0];
    let index_name = keyspace::ft_index_name(&scope, "facts");
    let hits = text
        // moon-v051-perf-exploit W0 fallout: the vendor bump added a `filter:
        // Option<&HybridFilter>` 8th param to `hybrid_search` (PR #174, HYBRID
        // FILTER on both branches). `None` preserves this test's pre-existing
        // unfiltered behavior byte-for-byte.
        .hybrid_search(&index_name, predicate, &embedding, "vec", None, 5, weights, None)
        .await
        .expect("hybrid_search must succeed once @content is in the schema (Gap 9 closure)");

    assert!(
        hits.iter().any(|h| h.key.as_bytes().ends_with(&id) || h.key.contains(&hex::encode(&id))),
        "seeded fact must appear in HYBRID hits — got {} hits, none matching marker",
        hits.len()
    );
}

#[tokio::test]
async fn graph_query_valid_at_round_trip_via_moon() {
    let scope = test_scope("graph-validat");
    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6380");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let id_hex = hex::encode(&id);
    storage
        .atomic_write(
            &scope,
            &[WriteOp::GraphNode {
                graph: "ignored-by-moon-scope-routing".into(),
                id,
                label: "Person".into(),
                props: serde_json::json!({
                    "id_hex": id_hex,
                    "name": "Moon ValidAt",
                    "type": "Person",
                }),
                index_kind: "entities".into(),
            }],
        )
        .await
        .expect("seed graph node");

    let as_of = HlcClock::new(0).tick();
    let q = CypherQuery {
        graph: "ignored-by-moon-scope-routing".into(),
        cypher: format!(
            "MATCH (n) WHERE n.id_hex='{id_hex}' RETURN n.id_hex AS id, n.name AS name, n.type AS type"
        ),
        params: serde_json::Map::new(),
    };

    let result = storage
        .graph_traverse(&scope, &q, Some(as_of))
        .await
        .expect("GRAPH.QUERY VALID_AT must succeed through Moon");
    assert_eq!(result.headers, vec!["id", "name", "type"]);
    assert!(
        result.rows.iter().any(|row| row.first().and_then(|v| v.as_str()) == Some(id_hex.as_str())),
        "seeded graph node must be visible through VALID_AT query: {result:?}"
    );
}

#[tokio::test]
async fn vector_search_as_of_round_trip_via_moon() {
    let scope = test_scope("vector-asof");
    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6380");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let marker = format!("moon-it-vector-asof-{}", ulid::Ulid::new());
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) * 0.0005).collect();
    storage
        .atomic_write(
            &scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: id.clone(),
                embedding: embedding.clone(),
                metadata: serde_json::json!({
                    "text": marker,
                    "source": "moon-it",
                    "valid_time_ms": HlcClock::new(0).tick().wall_ms,
                }),
            }],
        )
        .await
        .expect("seed vector chunk");

    let as_of = HlcClock::new(0).tick();
    let hits = storage
        .vector_search(&scope, "chunks", &embedding, 5, None, Some(as_of), false)
        .await
        .expect("FT.SEARCH KNN AS_OF must succeed through Moon");

    assert!(
        hits.iter().any(|h| h.id == id),
        "seeded vector chunk must be visible through FT.SEARCH AS_OF: {hits:?}"
    );
}

#[tokio::test]
async fn keyword_search_as_of_round_trip_via_moon() {
    let scope = test_scope("keyword-asof");
    let storage = MoonStorage::connect(&moon_url())
        .await
        .expect("connect to Moon — set MOON_URL env or run Moon at localhost:6380");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let marker = format!("moon-it-keyword-asof-{}", ulid::Ulid::new());
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) * 0.0007).collect();
    storage
        .atomic_write(
            &scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: id.clone(),
                embedding,
                metadata: serde_json::json!({
                    "text": marker,
                    "source": "moon-it",
                    "valid_time_ms": HlcClock::new(0).tick().wall_ms,
                }),
            }],
        )
        .await
        .expect("seed keyword chunk");

    let as_of = HlcClock::new(0).tick();
    let hits = storage
        .keyword_search(&scope, "chunks", &marker, 5, None, Some(as_of))
        .await
        .expect("FT.SEARCH BM25 AS_OF must succeed through Moon");

    assert!(
        hits.iter().any(|h| h.id == id),
        "seeded keyword chunk must be visible through FT.SEARCH AS_OF: {hits:?}"
    );
}

/// F11 guard — no test in this file may go back to the shared partition.
///
/// The fix above is invisible in behaviour: with every test on its own scope
/// the suite is green, and it would also be green the day someone reintroduces
/// `Scope::dev()` here — right up until the index grows past the top-k and the
/// flake comes back. The failure took three consecutive runs to appear once;
/// a reviewer will not reproduce it, so the guard has to be structural.
///
/// It reads the source rather than the behaviour on purpose, and pairs with
/// the measured proof in the commit that introduced it (ten consecutive green
/// runs against one Moon, where run three had been red).
#[test]
fn no_test_in_this_file_uses_the_shared_dev_scope() {
    let src = include_str!("moon_client_smoke.rs");
    // Assembled so this assertion cannot match its own text.
    let needle = format!("Scope{}dev()", "::");

    let offenders: Vec<String> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            // Prose may name the symbol — the module docs explain the bug.
            !t.starts_with("//") && !t.starts_with('*') && l.contains(&needle)
        })
        .map(|(i, l)| format!("{}: {}", i + 1, l.trim()))
        .collect();

    assert!(
        offenders.is_empty(),
        "these lines put a test back on the shared dev partition: {offenders:#?}. \
         Moon namespaces the vector index and the graph by scope, so a shared \
         scope means every test's rows compete in one top-k — which is green \
         until the index outgrows the k, then flaky forever. Use test_scope(..)."
    );

    // Vacuity floor: if this file is ever restructured so the scan walks
    // nothing, fail here rather than reporting a clean bill of health.
    let scoped = src.matches("test_scope(").count();
    assert!(
        scoped >= 7,
        "expected at least 7 test_scope( sites (6 tests + the helper), found {scoped} — \
         this guard is probably scanning a file that no longer looks like it thinks"
    );
}
