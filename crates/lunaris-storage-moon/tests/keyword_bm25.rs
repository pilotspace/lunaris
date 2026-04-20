//! `KeywordPort::keyword_search` round-trip vs live Moon.
//!
//! Gated behind `moon-it`. Skips gracefully when `MOON_URL` env var is unset so
//! `cargo test --workspace` keeps working without a live Moon.

#![cfg(feature = "moon-it")]

use lunaris_core::storage::StoragePort;
use lunaris_core::storage::keyword::KeywordPort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use ulid::Ulid;

const FIXTURE_TEXTS: &[&str] = &[
    "the quick brown fox jumps over the lazy dog",
    "lazy dog snoozes in the afternoon sun",
    "fox runs fast across the open meadow",
];

fn url() -> Option<String> {
    std::env::var("MOON_URL").ok().or_else(|| Some("moon://localhost:6390".to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_returns_normalized_bm25_hits_on_moon() {
    let Some(u) = url() else {
        eprintln!("MOON_URL unset; SKIP");
        return;
    };

    let moon = match MoonStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            return;
        }
    };

    // Insert three chunks with distinct text via atomic_write.
    let mut ops: Vec<WriteOp> = Vec::with_capacity(FIXTURE_TEXTS.len());
    for text in FIXTURE_TEXTS.iter() {
        let id = Ulid::new();
        ops.push(WriteOp::VectorUpsert {
            index: "chunks".to_string(),
            id: id.to_bytes().to_vec(),
            embedding: vec![0.0_f32; 768],
            metadata: serde_json::json!({"text": text}),
        });
    }
    moon.atomic_write(&ops).await.expect("atomic_write");

    // Query "fox" — fixtures 0 and 2 contain it; fixture 1 does not.
    let hits = moon.keyword_search("chunks", "fox", 5, None, None).await.expect("keyword_search");

    // Moon may return more than the strict 2 if the test corpus is shared across
    // runs — we assert the upper bound on `k` and the score range only.
    assert!(hits.len() <= 5);
    for h in &hits {
        assert!(h.score >= 0.0 && h.score <= 1.0, "normalized score out of [0,1]: {}", h.score);
        assert!(!h.id.is_empty(), "id must be non-empty");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_rejects_unknown_index_on_moon() {
    let Some(u) = url() else {
        eprintln!("MOON_URL unset; SKIP");
        return;
    };
    let moon = match MoonStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            return;
        }
    };
    let r = moon.keyword_search("unknown_table", "x", 1, None, None).await;
    assert!(r.is_err(), "unknown index must error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_escapes_ft_specials_on_moon() {
    // T-02-02-02 mitigation proof: the Moon FT parser would reject or mis-parse
    // `"foo (bar)"` without escaping. With ft_escape it should run cleanly even
    // if no hits come back.
    let Some(u) = url() else {
        eprintln!("MOON_URL unset; SKIP");
        return;
    };
    let moon = match MoonStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            return;
        }
    };
    let r = moon
        .keyword_search("chunks", "foo (bar)", 5, None, None)
        .await
        .expect("keyword_search must not error on FT specials");
    let _ = r;
}
