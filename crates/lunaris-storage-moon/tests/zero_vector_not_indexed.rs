//! F22 — a document Lunaris failed to embed must not outrank every real match.
//!
//! Gated behind `moon-it`. To run:
//!
//! ```bash
//! MOON_TEST_BINARY=/path/to/moon \
//!   cargo test -p lunaris-storage-moon --features moon-it \
//!   --test zero_vector_not_indexed -- --nocapture
//! ```
//!
//! ## The defect
//!
//! Moon's KNN `__vec_score` is a DISTANCE and the adapter converts it with
//! `1/(1+d)`. A zero vector sits at distance `||q||` from any query — 1.0 for
//! the unit vectors every real embedder produces — while a genuine but weak
//! match sits further away. Measured on a live Moon before the fix, against a
//! random unit query: the all-zero row came back FIRST at d=1.0, ahead of real
//! chunks at 1.765 and 1.810.
//!
//! Zero vectors are not neutral. They are a better-than-average match to
//! everything, and the documented Tier-0 build produces them: `resolve_embedder`
//! falls back to `NoopEmbedder` when no GGUF is staged and no remote embedder is
//! configured. `lunaris-hook` capture writes them deliberately and promotes real
//! ones later — a window that never closes if the promotion worker is off.
//!
//! ## What these tests assert
//!
//! Behaviour against a live Moon, not the source text. `atomic.rs`'s existing
//! `valid_time_tests` are `include_str!` + `contains` greps, which pass over a
//! broken implementation; that pattern is deliberately not reused here.

#![cfg(feature = "moon-it")]

use lunaris_core::{Scope, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;
use lunaris_test_harness::EphemeralMoon;

mod common;

const DIM: usize = 768;

async fn private_moon(test: &str) -> Option<EphemeralMoon> {
    match EphemeralMoon::spawn().await {
        Ok(m) => Some(m),
        Err(e) => {
            common::note_moon_unreachable(format!("{test}: no ephemeral Moon ({e})"));
            None
        }
    }
}

/// A unit vector with a distinctive direction, so a self-match is unambiguous.
fn real_vector(seed: f32) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[0] = 1.0;
    for (i, x) in v.iter_mut().enumerate().skip(1) {
        *x = seed * (i as f32) * 1e-6;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / norm).collect()
}

fn upsert(id: &[u8], embedding: &[f32], text: &str) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "chunks".into(),
        id: id.to_vec(),
        embedding: embedding.to_vec(),
        metadata: serde_json::json!({ "text": text }),
    }
}

/// The headline: an un-embedded row must not come back at all, while a real
/// one still does.
///
/// Both halves are load-bearing. Without the second assertion, "the zero row
/// is absent" is satisfied by a search that returns nothing whatsoever — an
/// empty index and a working guard look identical from the first assertion
/// alone.
#[tokio::test]
async fn a_zero_embedding_is_not_indexed_while_a_real_one_is() {
    let Some(moon) = private_moon("a_zero_embedding_is_not_indexed_while_a_real_one_is").await
    else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f22-{}", ulid::Ulid::new())).expect("scope name valid");

    let zero_id = ulid::Ulid::new().to_bytes().to_vec();
    let real_id = ulid::Ulid::new().to_bytes().to_vec();
    let real = real_vector(1.0);

    storage
        .atomic_write(
            &scope,
            &[
                upsert(&zero_id, &vec![0.0_f32; DIM], "un-embedded row"),
                upsert(&real_id, &real, "genuinely embedded row"),
            ],
        )
        .await
        .expect("a zero embedding must still be ACCEPTED — it is skipped, not rejected");

    // Query along a direction unrelated to `real`, which is the case that used
    // to favour the zero row most strongly: the weaker the genuine match, the
    // more decisively distance 1.0 beat it.
    let query = real_vector(-1.0);
    let hits = storage
        .vector_search(&scope, "chunks", &query, 50, None, None, false)
        .await
        .expect("vector_search must succeed");

    assert!(
        hits.iter().any(|h| h.id == real_id),
        "the genuinely embedded row is missing, so this test proves nothing about the zero row. \
         got {} hits: {:?}",
        hits.len(),
        hits.iter().map(|h| hex::encode(&h.id)).collect::<Vec<_>>()
    );
    assert!(
        !hits.iter().any(|h| h.id == zero_id),
        "F22: the un-embedded row entered the KNN index. Under 1/(1+d) a zero vector sits at \
         distance ||q|| from every query and outranks genuine matches permanently. It must be \
         written WITHOUT the `vec` field. got {} hits: {:?}",
        hits.len(),
        hits.iter().map(|h| hex::encode(&h.id)).collect::<Vec<_>>()
    );
}

/// The capture-then-promote flow `lunaris-hook` depends on must keep working:
/// a row skipped at capture joins the index once a real vector arrives.
#[tokio::test]
async fn a_skipped_row_joins_the_index_when_its_vector_is_promoted() {
    let Some(moon) =
        private_moon("a_skipped_row_joins_the_index_when_its_vector_is_promoted").await
    else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f22-{}", ulid::Ulid::new())).expect("scope name valid");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let real = real_vector(1.0);

    // Capture: NoopEmbedder, zero vector, skipped.
    storage
        .atomic_write(&scope, &[upsert(&id, &vec![0.0_f32; DIM], "captured")])
        .await
        .expect("capture write must succeed");
    let before = storage
        .vector_search(&scope, "chunks", &real, 50, None, None, false)
        .await
        .expect("vector_search must succeed");
    assert!(!before.iter().any(|h| h.id == id), "the captured row must not be in KNN yet");

    // Promotion: the worker re-upserts the SAME id with a real vector.
    storage
        .atomic_write(&scope, &[upsert(&id, &real, "captured")])
        .await
        .expect("promotion write must succeed");
    let after = storage
        .vector_search(&scope, "chunks", &real, 50, None, None, false)
        .await
        .expect("vector_search must succeed");
    assert!(
        after.iter().any(|h| h.id == id),
        "promotion did not bring the row into the KNN index — the capture-then-promote flow in \
         lunaris-hook is broken. got {} hits",
        after.len()
    );
}

/// A NaN embedding is skipped too: it makes every comparison against the row
/// meaningless rather than merely wrong.
#[tokio::test]
async fn a_non_finite_embedding_is_not_indexed() {
    let Some(moon) = private_moon("a_non_finite_embedding_is_not_indexed").await else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f22-{}", ulid::Ulid::new())).expect("scope name valid");

    let nan_id = ulid::Ulid::new().to_bytes().to_vec();
    let real_id = ulid::Ulid::new().to_bytes().to_vec();
    let real = real_vector(1.0);
    let mut nan_vec = real_vector(2.0);
    nan_vec[3] = f32::NAN;

    storage
        .atomic_write(
            &scope,
            &[upsert(&nan_id, &nan_vec, "nan row"), upsert(&real_id, &real, "real row")],
        )
        .await
        .expect("a non-finite embedding must be accepted and skipped, not rejected");

    let hits = storage
        .vector_search(&scope, "chunks", &real, 50, None, None, false)
        .await
        .expect("vector_search must succeed");
    assert!(hits.iter().any(|h| h.id == real_id), "vacuity floor: the real row must be present");
    assert!(!hits.iter().any(|h| h.id == nan_id), "a non-finite embedding must not be indexed");
}
