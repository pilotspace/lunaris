//! Integration test — the Moon adapter creates its FT vector indices at the
//! configured embedder dimension, not a hard-coded 768.
//!
//! Gated behind the `moon-it` Cargo feature so CI without a reachable Moon
//! instance does NOT fail on the per-commit gate. To run:
//!
//! ```bash
//! MOON_URL=moon://localhost:6380 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test dim_configurable -- --nocapture
//! ```
//!
//! Moon's `FT.CREATE` has no dimension cap (`DIM > 0` is the only constraint),
//! so a 1536-d embedder (OpenAI `text-embedding-3`) works out of the box once
//! `MoonStorage::connect_with_dim` sizes the index to match.
//!
//! ## Why a fresh random scope per run
//!
//! Moon's `FT.CREATE` is idempotent and never resizes an existing index. If a
//! prior run created a 768-d index under some scope, reopening that scope at
//! 1536-d would silently keep the 768-d index and the write would mismatch.
//! Using a random scope per run guarantees we exercise the fresh-index path.

#![cfg(feature = "moon-it")]

use lunaris_core::{Scope, StorageError, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".into())
}

/// `connect_with_dim(url, 0)` MUST be rejected before any network IO.
#[tokio::test]
async fn connect_with_dim_zero_is_rejected() {
    let r = MoonStorage::connect_with_dim(&moon_url(), 0).await;
    match r {
        Err(StorageError::Backend(msg)) => {
            assert!(msg.contains("vector dim must be > 0"), "unexpected msg: {msg}");
        }
        other => panic!("expected Backend(vector dim must be > 0), got {other:?}"),
    }
}

/// A 1536-d embedder works against live Moon: connect at 1536, write a 1536-d
/// vector under a fresh scope, then vector_search for it and get a hit.
#[tokio::test]
async fn vector_upsert_and_search_at_1536_dim() {
    const DIM: usize = 1536;

    let storage = MoonStorage::connect_with_dim(&moon_url(), DIM)
        .await
        .expect("connect_with_dim(1536) to live Moon — set MOON_URL or run Moon at localhost:6380");

    // Capabilities now report the configured dim.
    assert_eq!(storage.capabilities().max_vector_dim, DIM as u32);

    // Fresh random scope so we never collide with a 768-d index from an
    // earlier test run (Moon's FT.CREATE is idempotent and never resizes).
    let scope = Scope::new(format!("dim-it-{}", ulid::Ulid::new())).expect("scope name valid");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let marker = "moon-it-dim-1536-marker";
    // Deterministic-ish 1536-d vector — first component large so cosine sim to
    // the same query is ~1.0.
    let mut embedding: Vec<f32> = vec![0.0; DIM];
    embedding[0] = 1.0;
    for (i, v) in embedding.iter_mut().enumerate().skip(1) {
        *v = (i as f32) * 1e-6;
    }

    storage
        .atomic_write(
            &scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: id.clone(),
                embedding: embedding.clone(),
                metadata: serde_json::json!({ "text": marker }),
            }],
        )
        .await
        .expect("atomic_write VectorUpsert with a 1536-d vector must succeed");

    let hits = storage
        .vector_search(&scope, "chunks", &embedding, 5, None, None, false)
        .await
        .expect("vector_search at 1536-d must succeed");

    assert!(
        !hits.is_empty(),
        "1536-d vector_search returned zero hits — the FT index did not accept the wider vector"
    );
    assert!(
        hits.iter().any(|h| h.id == id),
        "seeded 1536-d vector (id {}) must appear in vector_search hits; got {} hits with ids {:?}",
        hex::encode(&id),
        hits.len(),
        hits.iter().map(|h| hex::encode(&h.id)).collect::<Vec<_>>()
    );
}

/// Phase 22 dim-guardrail — once the global `chunks` index exists at dim N,
/// reopening `MoonStorage::connect_with_dim(url, M)` with `M != N` MUST
/// surface a `StorageError::Backend("vector dim mismatch ...")` error before
/// any `atomic_write` runs. Without the guardrail, FT.CREATE silently keeps
/// the old dim and the first vector write blows up much later.
///
/// This test relies on a prior connect (any other test in this file or a
/// shared Moon instance) having created `chunks` at SOME dim. If `chunks` is
/// absent the test self-creates it via a first `connect_with_dim(_, 768)`,
/// then re-opens at 1024 and asserts the mismatch.
#[tokio::test]
async fn reopen_at_mismatched_dim_is_rejected() {
    // Step 1: ensure the global `chunks` / `entities` / `facts` /
    // `communities` indices exist at 768d. Idempotent on Moon — if a prior
    // test already created them at 768, this is a no-op.
    let _baseline = MoonStorage::connect_with_dim(&moon_url(), 768)
        .await
        .expect("baseline connect_with_dim(768) — needs MOON_URL or Moon at localhost:6380");
    drop(_baseline);

    // Step 2: reopen at 1024 against the same instance. The dim guardrail
    // must reject before ensure_indexes runs.
    let r = MoonStorage::connect_with_dim(&moon_url(), 1024).await;
    match r {
        Err(StorageError::Backend(msg)) => {
            assert!(
                msg.contains("vector dim mismatch"),
                "expected 'vector dim mismatch' in error, got: {msg}"
            );
            assert!(
                msg.contains("1024") && msg.contains("768"),
                "error must echo both wanted (1024) and existing (768) dims: {msg}"
            );
            assert!(
                msg.contains("FT.DROPINDEX"),
                "error must point operator at the recovery command: {msg}"
            );
        }
        other => panic!("expected Backend(vector dim mismatch), got {other:?}"),
    }
}
