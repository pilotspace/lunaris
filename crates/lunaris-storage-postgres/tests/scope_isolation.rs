//! Cross-scope leak integration test — RFC 0001 Wave 1B mandatory gate.
//!
//! Proves that Postgres RLS isolation works:
//!
//! 1. Insert an `Episode` (via `KvPut`) under `scope = "tenant_a"`.
//! 2. `read_as_of` with `scope = "tenant_b"` MUST return `None`.
//! 3. `vector_search` with `scope = "tenant_b"` MUST return zero hits.
//! 4. `read_as_of` with `scope = "tenant_a"` MUST return the row.
//!
//! The test also verifies that `VectorUpsert` under `tenant_a` is invisible
//! to `vector_search` under `tenant_b` (isolates the RLS on `chunks` table).
//!
//! Gated behind the `pg-it` Cargo feature — requires a live Postgres instance
//! at `PG_URL` (default `postgres://localhost/lunaris`) with pgvector + AGE +
//! pgmq extensions available.
//!
//! ```bash
//! cargo test -p lunaris-storage-postgres --features pg-it --test scope_isolation
//! ```

#![cfg(feature = "pg-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, Scope, StoragePort, WriteOp};
use lunaris_storage_postgres::PostgresStorage;
use ulid::Ulid;

fn pg_url() -> String {
    std::env::var("PG_URL").unwrap_or_else(|_| "postgres://localhost/lunaris".into())
}

/// Cross-scope leak gate — MANDATORY per RFC 0001 §3.5 Wave 1B.
///
/// Inserts one row under `tenant_a` via `KvPut` and one chunk via
/// `VectorUpsert`, then asserts that reads under `tenant_b` see zero rows.
/// Reads under `tenant_a` MUST return the row — proving RLS is not vacuously
/// blocking everything.
#[tokio::test]
async fn cross_scope_read_returns_zero_for_wrong_scope() {
    let s = match PostgresStorage::connect(&pg_url()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP scope_isolation test");
            return;
        }
    };

    let scope_a = Scope::new("tenant_a").expect("tenant_a is a valid scope");
    let scope_b = Scope::new("tenant_b").expect("tenant_b is a valid scope");

    let clock = HlcClock::new(0);

    // --- write an Episode under scope_a ---
    let ep =
        Episode::new(scope_a.clone(), "scope_isolation.md", "secret data for tenant A", &clock);
    let key = format!("lunaris:episode:scope-isolation:{}", ep.id).into_bytes();
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    let lsn = s
        .atomic_write(&scope_a, &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write under scope_a must succeed");
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "LSN must be non-zero: {lsn:?}");

    let as_of = clock.tick();

    // --- read back under scope_b: MUST return None ---
    let row_b =
        s.read_as_of(&scope_b, &key, as_of).await.expect("read_as_of under scope_b must not error");
    assert!(
        row_b.is_none(),
        "RLS LEAK: read_as_of(&scope_b) returned Some — cross-scope data visible. \
         RFC 0001 §3.5 isolation gate FAILED."
    );

    // --- read back under scope_a: MUST return the row ---
    let row_a = s
        .read_as_of(&scope_a, &key, as_of)
        .await
        .expect("read_as_of under scope_a must not error")
        .expect("read_as_of(&scope_a) must find the written row");
    assert_eq!(
        row_a.value,
        Bytes::from(value.clone()),
        "value must round-trip byte-identically under scope_a"
    );

    // Deserialize and verify key fields.
    let back: Episode = serde_json::from_slice(&row_a.value).expect("episode deserializes");
    assert_eq!(back.id, ep.id, "episode id must round-trip");
    assert_eq!(back.source, ep.source, "episode source must round-trip");

    eprintln!("scope_isolation KvPut: scope_b read = None ✓  scope_a read = Some({}) ✓", ep.id);
}

/// Cross-scope vector search leak gate — RFC 0001 §3.5.
///
/// Inserts a chunk embedding under `tenant_a` via `VectorUpsert`, then
/// asserts that `vector_search` under `tenant_b` returns zero hits.
/// `vector_search` under `tenant_a` MUST return at least one hit.
#[tokio::test]
async fn cross_scope_vector_search_returns_zero_for_wrong_scope() {
    let s = match PostgresStorage::connect(&pg_url()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP scope_isolation vector test");
            return;
        }
    };

    let scope_a = Scope::new("tenant_a").expect("valid");
    let scope_b = Scope::new("tenant_b").expect("valid");

    let clock = HlcClock::new(0);

    // Use a unique chunk ID so parallel test runs don't interfere.
    let chunk_id = Ulid::new();
    // Simple deterministic embedding — all 1.0 so cosine distance to a query
    // of all 1.0 is 0 (perfect match).
    let embedding = vec![1.0_f32; 768];

    let lsn = s
        .atomic_write(
            &scope_a,
            &[WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id: chunk_id.to_bytes().to_vec(),
                embedding: embedding.clone(),
                metadata: serde_json::json!({
                    "text": "secret chunk for tenant A",
                    "scope_test": true,
                }),
            }],
        )
        .await
        .expect("VectorUpsert under scope_a must succeed");
    assert!(lsn.wall_ms > 0 || lsn.counter > 0, "LSN must be non-zero: {lsn:?}");

    let as_of_hlc = clock.tick();

    // Query vector identical to the embedding — should be distance 0 for the inserted chunk.
    let query = vec![1.0_f32; 768];

    // --- vector_search under scope_b: MUST return zero hits ---
    let hits_b = s
        .vector_search(&scope_b, "chunks", &query, 10, None, Some(as_of_hlc), false)
        .await
        .expect("vector_search under scope_b must not error");

    // Filter to just our chunk_id to avoid interference from other test data.
    let chunk_id_bytes = chunk_id.to_bytes().to_vec();
    let our_hits_b: Vec<_> = hits_b.iter().filter(|h| h.id == chunk_id_bytes).collect();

    assert!(
        our_hits_b.is_empty(),
        "RLS LEAK: vector_search(&scope_b) returned {} hit(s) for our chunk — \
         cross-scope data visible. RFC 0001 §3.5 isolation gate FAILED.",
        our_hits_b.len()
    );

    // --- vector_search under scope_a: MUST find our chunk ---
    let hits_a = s
        .vector_search(&scope_a, "chunks", &query, 10, None, Some(as_of_hlc), false)
        .await
        .expect("vector_search under scope_a must not error");

    let our_hits_a: Vec<_> = hits_a.iter().filter(|h| h.id == chunk_id_bytes).collect();

    assert!(
        !our_hits_a.is_empty(),
        "vector_search(&scope_a) returned zero hits for our chunk — \
         RLS may be blocking the writer's own reads. scope_a must see its own data."
    );

    eprintln!(
        "scope_isolation VectorSearch: scope_b hits = {} ✓  scope_a hits = {} ✓",
        our_hits_b.len(),
        our_hits_a.len()
    );
}
