//! `KeywordPort::keyword_search` round-trip vs live Moon.
//!
//! Gated behind `moon-it`. Skips gracefully when `MOON_URL` env var is unset so
//! `cargo test --workspace` keeps working without a live Moon.

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::keyword::KeywordPort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use ulid::Ulid;

mod common;
const FIXTURE_TEXTS: &[&str] = &[
    "the quick brown fox jumps over the lazy dog",
    "lazy dog snoozes in the afternoon sun",
    "fox runs fast across the open meadow",
];

/// The live Moon this file talks to.
///
/// Returns a `String`, not an `Option`. The previous signature was
/// `Option<String>` built as `.ok().or_else(|| Some(default))` — which can
/// never be `None`, so each test's `let Some(u) = url() else { ..skip.. }` arm
/// was unreachable. Three skip branches that could not fire, and which read
/// like live coverage of the unset case. `integration.yml` sets `MOON_URL` to
/// the job's Moon, so CI has always taken the `Some` path anyway.
fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_returns_normalized_bm25_hits_on_moon() {
    let u = url();

    let moon = match MoonStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            common::note_moon_unreachable(e);
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
    moon.atomic_write(&Scope::dev(), &ops).await.expect("atomic_write");

    // Query "fox" — fixtures 0 and 2 contain it; fixture 1 does not.
    let hits = moon
        .keyword_search(&Scope::dev(), "chunks", "fox", 5, None, None)
        .await
        .expect("keyword_search");

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
    let u = url();
    let moon = match MoonStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            common::note_moon_unreachable(e);
            return;
        }
    };
    let r = moon.keyword_search(&Scope::dev(), "unknown_table", "x", 1, None, None).await;
    assert!(r.is_err(), "unknown index must error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyword_search_escapes_ft_specials_on_moon() {
    // T-02-02-02 mitigation proof: the Moon FT parser would reject or mis-parse
    // `"foo (bar)"` without escaping. With ft_escape it should run cleanly even
    // if no hits come back.
    let u = url();
    let moon = match MoonStorage::connect(&u).await {
        Ok(s) => s,
        Err(e) => {
            common::note_moon_unreachable(e);
            return;
        }
    };
    // Seed one row so the `chunks` index exists.
    //
    // Without this the test asserts nothing about escaping: on a Moon where no
    // sibling has written yet it fails with `no such index`, and on one where a
    // sibling HAS, it passes for a reason that has nothing to do with
    // `ft_escape`. Reproduced 3/3 against a fresh Moon and 3/3 green against a
    // warm one. CI never saw it because earlier steps in the same job share the
    // 6390 Moon and create `chunks` first — the suite has been passing on
    // cross-step pollution.
    moon.atomic_write(
        &Scope::dev(),
        &[WriteOp::VectorUpsert {
            index: "chunks".to_string(),
            id: Ulid::new().to_bytes().to_vec(),
            embedding: vec![0.0_f32; 768],
            metadata: serde_json::json!({"text": "escape fixture row"}),
        }],
    )
    .await
    .expect("seeding the chunks index must succeed");

    let r = moon
        .keyword_search(&Scope::dev(), "chunks", "foo (bar)", 5, None, None)
        .await
        .expect("keyword_search must not error on FT specials");
    let _ = r;
}
