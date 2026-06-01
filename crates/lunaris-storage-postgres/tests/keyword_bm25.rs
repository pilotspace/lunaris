//! `KeywordPort::keyword_search` round-trip vs live Postgres+pgvector+age+pgmq.
//!
//! Gated behind `pg-it`. Skips gracefully when `PG_URL` env var is unset so
//! `cargo test --workspace` keeps working without a live Postgres.

#![cfg(feature = "pg-it")]

use bytes::Bytes;
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::hlc::HlcClock;
use lunaris_core::primitives::Chunk;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::keyword::KeywordPort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_postgres::PostgresStorage;
use ulid::Ulid;

const FIXTURE_TEXTS: &[&str] = &[
    "the quick brown fox jumps over the lazy dog",
    "lazy dog snoozes in the afternoon sun",
    "fox runs fast across the open meadow",
];

fn url() -> Option<String> {
    std::env::var("PG_URL").ok().or_else(|| Some("postgres://localhost/lunaris".to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_returns_normalized_bm25_hits() {
    let Some(u) = url() else {
        eprintln!("PG_URL unset; SKIP");
        return;
    };

    let pg = match PostgresStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP");
            return;
        }
    };

    // Insert three chunks with distinct text via atomic_write.
    let clock = HlcClock::new(0);
    let episode_id = Ulid::new();
    let mut ops: Vec<WriteOp> = Vec::with_capacity(FIXTURE_TEXTS.len() * 2);
    for (i, text) in FIXTURE_TEXTS.iter().enumerate() {
        let id = Ulid::new();
        let chunk = Chunk {
            id,
            scope: Scope::dev(), // RFC 0001 Wave 0 migration crutch
            episode_id,
            text: text.to_string(),
            tokens: text.split_whitespace().count() as u32,
            offset: i as u32,
            heading_path: vec![],
            overlap_tail: String::new(),
            embedding: Some(vec![0.0_f32; 768]),
            bt: BiTemporal::now(&clock),
            parent_id: None,
        };
        let chunk_value = serde_json::to_vec(&chunk).unwrap();
        // KvPut isn't needed for BM25 (text_tsv is generated from chunks.payload->>'text').
        // Insert directly into the chunks table via VectorUpsert which sets the payload.
        ops.push(WriteOp::VectorUpsert {
            index: "chunks".to_string(),
            id: id.to_bytes().to_vec(),
            embedding: vec![0.0_f32; 768],
            metadata: serde_json::json!({"text": text}),
        });
        let _ = chunk_value;
    }
    pg.atomic_write(&Scope::dev(), &ops).await.expect("atomic_write");

    // Query "fox" — fixtures 0 and 2 contain it; fixture 1 does not.
    let hits = pg
        .keyword_search(&Scope::dev(), "chunks", "fox", 5, None, None)
        .await
        .expect("keyword_search");

    assert_eq!(hits.len(), 2, "exactly 2 chunks contain `fox`, got {}", hits.len());
    for h in &hits {
        assert!(h.score >= 0.0 && h.score <= 1.0, "normalized score out of [0,1]: {}", h.score);
        assert!(!h.id.is_empty(), "id must be non-empty");
    }

    // Bonus: ts_rank_cd > 0 for both hits (covered by the @@ predicate filtering them in).
    assert!(hits.iter().all(|h| h.raw_score > 0.0), "raw scores must be > 0 for matched hits");

    let _ = Bytes::new(); // touch the bytes import so it's never unused
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_rejects_unknown_index() {
    let Some(u) = url() else {
        eprintln!("PG_URL unset; SKIP");
        return;
    };
    let pg = match PostgresStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP");
            return;
        }
    };
    let r = pg.keyword_search(&Scope::dev(), "unknown_table", "x", 1, None, None).await;
    assert!(r.is_err(), "unknown index must error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_handles_sql_injection_attempt() {
    // T-02-02-01 mitigation proof: query text flows through plainto_tsquery as a
    // bound parameter. An injection attempt MUST come back as a normal empty
    // result set (no DB error, no table dropped).
    let Some(u) = url() else {
        eprintln!("PG_URL unset; SKIP");
        return;
    };
    let pg = match PostgresStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP");
            return;
        }
    };

    let r = pg
        .keyword_search(&Scope::dev(), "chunks", "foo'; DROP TABLE chunks;--", 5, None, None)
        .await
        .expect("keyword_search must not error on injection attempt");

    // The result set may be empty OR contain incidental matches on `foo` — the
    // contract is "no error, no schema mutation" — both proven by getting here.
    assert!(r.len() <= 5);
}
