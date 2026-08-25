//! F22 (residual half) — the rows written BEFORE the write-side guard landed.
//!
//! ## What is already fixed, and what this file is about
//!
//! `atomic.rs` no longer writes a `vec` field when `unindexable_reason` says the
//! embedding is all-zero or non-finite (commits `f073481` RED / `0a2d043`
//! GREEN). Every vector write in the workspace routes through
//! `WriteOp::VectorUpsert`, so no NEW row can reach the KNN index unindexable.
//!
//! That fix is forward-only. It does nothing for rows already on disk, and a
//! survey of the live store found 622 of 1235 chunk rows (50.4%) carrying an
//! all-zero `vec`, written continuously between 2026-07-20 and 2026-08-22. The
//! promotion worker in `lunaris-hook` cannot reach them either: it is driven by
//! `publish_capture_receipt` at capture time and only ever handles rows an
//! event was published for. It never scans. A row written before the guard has
//! no event and never will.
//!
//! ## Why a zero row is worse than a missing row
//!
//! Moon's chunk index is HNSW/COSINE and Lunaris reports `1/(1+d)`. A zero
//! vector has no direction, so Moon scores it at distance 1.0 — dead centre,
//! equidistant from everything. Measured on an ephemeral Moon against a unit
//! query:
//!
//! ```text
//!   cos 0.9 real match   score 0.832
//!   ALL-ZERO row         score 0.500     <-- outranks the next two
//!   cos 0.3 real match   score 0.416
//!   cos 0.0 real match   score 0.333
//! ```
//!
//! So the failure is not "one document is unreachable". It is that a document
//! Lunaris could not embed **outranks every real match below cosine 0.5**, on
//! every query, forever — and being content-independent it does so uniformly,
//! which is precisely the profile of a result nobody can explain from the text.
//!
//! ## The shape of the repair
//!
//! Bring the legacy rows into the post-guard shape: drop the `vec` field and
//! leave everything else. The row keeps its `meta` and `content`, so hydration
//! and BM25 still see it and a later re-embed can fill the vector back in —
//! deleting the row would lose the document. Repair is per-scope and defaults
//! to a dry run, because the operator running it against a production store
//! deserves the count before the mutation.

#![cfg(feature = "moon-it")]

use lunaris_core::{Scope, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_moon::keyspace::ft_index_name;
use lunaris_test_harness::EphemeralMoon;

mod common;

const DIM: usize = 768;

/// A unit vector with the given weights on the given axes. Weights are
/// normalised, so `axis(&[(0, 1.0)])` and the query below are the same ray and
/// the cosine similarity of any other row is readable straight off its weights.
fn axis(pairs: &[(usize, f32)]) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    for (i, w) in pairs {
        v[*i] = *w;
    }
    let norm: f32 = v.iter().map(|f| f * f).sum::<f32>().sqrt();
    for f in v.iter_mut() {
        *f /= norm;
    }
    v
}

fn query() -> Vec<f32> {
    axis(&[(0, 1.0)])
}

/// cos 0.3 against `query()` — a real, relevant, imperfect match. Below the
/// 0.5 cosine where a zero row takes over, which is where most real hits live.
fn mid_vector() -> Vec<f32> {
    axis(&[(0, 0.3), (1, 0.954)])
}

/// cos 0.0 against `query()` — a weak match that still belongs in the tail.
fn far_vector() -> Vec<f32> {
    axis(&[(1, 1.0)])
}

fn upsert(id: &[u8], embedding: Vec<f32>, text: &str) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "chunks".into(),
        id: id.to_vec(),
        embedding,
        metadata: serde_json::json!({
            "text": text,
            "source": "f22-corpus.md",
        }),
    }
}

fn row_key(scope: &Scope, id: &[u8]) -> String {
    format!("{}:{}", ft_index_name(scope, "chunks"), hex::encode(id))
}

async fn private_moon(test: &str) -> Option<EphemeralMoon> {
    match EphemeralMoon::spawn().await {
        Ok(m) => Some(m),
        Err(e) => {
            common::note_moon_unreachable(format!("{test}: no ephemeral Moon ({e})"));
            None
        }
    }
}

/// Writes a row the way Lunaris did BEFORE the guard: a real `VectorUpsert`
/// (so `meta`, `content` and the index entry are all built by the production
/// path), then the `vec` field overwritten with `DIM * 4` zero bytes.
///
/// It has to be done this way round. The landed guard means a zero embedding
/// handed to `VectorUpsert` no longer produces a `vec` field at all, so the
/// corrupt shape can no longer be reached through the public write path — the
/// only honest way to build the wreckage is by hand. (Same reasoning, and the
/// same `redis` dev-dependency, as `mq_stranded_recovery.rs`.)
async fn seed_legacy_zero_row(storage: &MoonStorage, scope: &Scope, id: &[u8]) {
    storage
        .atomic_write(scope, &[upsert(id, axis(&[(0, 1.0)]), "a document Lunaris failed to embed")])
        .await
        .expect("seeding the pre-corruption row must succeed");
    let mut typed = storage.client().typed();
    let replaced: i64 = typed
        .hset(row_key(scope, id).as_bytes(), "vec", vec![0u8; DIM * 4])
        .await
        .expect("overwriting `vec` with zeroes must succeed");
    assert_eq!(
        replaced, 0,
        "HSET must have REPLACED an existing `vec` field (0), not added a new one (1) — \
         if this is 1 the production write path never wrote a vector and the fixture \
         is not reproducing a legacy row at all"
    );
}

async fn ranked_ids(storage: &MoonStorage, scope: &Scope) -> Vec<Vec<u8>> {
    storage
        .vector_search(scope, "chunks", &query(), 10, None, None, false)
        .await
        .expect("vector_search must succeed")
        .into_iter()
        .map(|h| h.id)
        .collect()
}

fn rank_of(ranked: &[Vec<u8>], id: &[u8]) -> Option<usize> {
    ranked.iter().position(|r| r == id)
}

#[tokio::test]
async fn a_legacy_zero_vector_row_outranks_real_matches_until_it_is_repaired() {
    let Some(moon) = private_moon("legacy_zero_outranks").await else { return };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new("f22.repair").expect("valid scope");

    let mid = ulid::Ulid::new().to_bytes().to_vec();
    let far = ulid::Ulid::new().to_bytes().to_vec();
    let zero = ulid::Ulid::new().to_bytes().to_vec();
    storage
        .atomic_write(
            &scope,
            &[
                upsert(&mid, mid_vector(), "a real match at cosine 0.3"),
                upsert(&far, far_vector(), "a weak match at cosine 0.0"),
            ],
        )
        .await
        .expect("seeding the real corpus must succeed");
    seed_legacy_zero_row(&storage, &scope, &zero).await;

    // The fixture has to reproduce the hazard before the repair can be said to
    // have removed it. If the zero row did not outrank the real ones here, this
    // test would pass after a repair that did nothing at all.
    let before = ranked_ids(&storage, &scope).await;
    assert_eq!(before.len(), 3, "all three rows must be KNN candidates before repair");
    let (zero_rank, mid_rank, far_rank) = (
        rank_of(&before, &zero).expect("the zero row must be a candidate before repair"),
        rank_of(&before, &mid).expect("the cos-0.3 row must be a candidate"),
        rank_of(&before, &far).expect("the cos-0.0 row must be a candidate"),
    );
    assert!(
        zero_rank < mid_rank && zero_rank < far_rank,
        "FIXTURE INVALID: the all-zero row ranked {zero_rank}, behind the real matches at \
         {mid_rank}/{far_rank}. This test only means something if the corrupt row actually \
         wins — check that Moon still scores a zero `vec` at distance 1.0"
    );

    let report = storage
        .repair_unindexable_vectors(&scope, "chunks", false)
        .await
        .expect("repair must succeed");
    assert_eq!(report.scanned, 3, "the repair must have walked every row in the scope");
    assert_eq!(report.unindexable, 1, "exactly one row carries an all-zero `vec`");
    assert_eq!(report.repaired, 1, "and that row must have been repaired");
    assert!(!report.dry_run);

    let after = ranked_ids(&storage, &scope).await;
    assert_eq!(
        rank_of(&after, &zero),
        None,
        "the repaired row must no longer be a KNN candidate — it has no direction to \
         match on, so any rank it holds is a rank it stole"
    );
    assert!(
        rank_of(&after, &mid).is_some() && rank_of(&after, &far).is_some(),
        "the repair must not have disturbed the real matches"
    );
}

#[tokio::test]
async fn repair_keeps_the_document_readable_by_hydration_and_keyword_search() {
    let Some(moon) = private_moon("repair_keeps_document").await else { return };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new("f22.preserve").expect("valid scope");

    let zero = ulid::Ulid::new().to_bytes().to_vec();
    seed_legacy_zero_row(&storage, &scope, &zero).await;

    let report = storage
        .repair_unindexable_vectors(&scope, "chunks", false)
        .await
        .expect("repair must succeed");
    assert_eq!(report.repaired, 1);

    // Dropping `vec` is the whole repair. The document itself is not the
    // problem and must survive: BM25 reads `content`, hydration reads `meta`,
    // and a later re-embed needs the row to still be there to write back into.
    let mut typed = storage.client().typed();
    let key = row_key(&scope, &zero);
    let meta: Option<String> =
        typed.hget(key.as_bytes(), "meta").await.expect("HGET meta must succeed");
    let content: Option<String> =
        typed.hget(key.as_bytes(), "content").await.expect("HGET content must succeed");
    let vec_field: Option<Vec<u8>> =
        typed.hget(key.as_bytes(), "vec").await.expect("HGET vec must succeed");

    assert!(meta.is_some(), "repair must not destroy `meta` — hydration reads it");
    assert!(
        content.as_deref().is_some_and(|c| c.contains("failed to embed")),
        "repair must not destroy `content` — BM25 scores against it, and it is the \
         only copy of the document text on this row"
    );
    assert!(vec_field.is_none(), "the all-zero `vec` field is what had to go");
}

#[tokio::test]
async fn a_dry_run_reports_the_damage_and_changes_nothing() {
    let Some(moon) = private_moon("dry_run_changes_nothing").await else { return };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new("f22.dryrun").expect("valid scope");

    let mid = ulid::Ulid::new().to_bytes().to_vec();
    let zero = ulid::Ulid::new().to_bytes().to_vec();
    storage
        .atomic_write(&scope, &[upsert(&mid, mid_vector(), "a real match at cosine 0.3")])
        .await
        .expect("seeding the real corpus must succeed");
    seed_legacy_zero_row(&storage, &scope, &zero).await;

    let report = storage
        .repair_unindexable_vectors(&scope, "chunks", true)
        .await
        .expect("dry run must succeed");
    assert!(report.dry_run);
    assert_eq!(report.scanned, 2);
    assert_eq!(report.unindexable, 1, "a dry run must still COUNT the damage");
    assert_eq!(report.repaired, 0, "…and must repair none of it");

    let after = ranked_ids(&storage, &scope).await;
    assert!(
        rank_of(&after, &zero).is_some(),
        "a dry run that actually mutated the store would be the worst possible bug in \
         this feature — the zero row must still be a candidate"
    );
    let mut typed = storage.client().typed();
    let vec_field: Option<Vec<u8>> =
        typed.hget(row_key(&scope, &zero).as_bytes(), "vec").await.expect("HGET vec must succeed");
    assert_eq!(
        vec_field.map(|v| v.len()),
        Some(DIM * 4),
        "the zero `vec` field must be byte-for-byte still there after a dry run"
    );
}

#[tokio::test]
async fn repair_never_reaches_outside_the_scope_it_was_given() {
    let Some(moon) = private_moon("repair_stays_in_scope").await else { return };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let mine = Scope::new("f22.tenant-a").expect("valid scope");
    let theirs = Scope::new("f22.tenant-b").expect("valid scope");

    let my_zero = ulid::Ulid::new().to_bytes().to_vec();
    let their_zero = ulid::Ulid::new().to_bytes().to_vec();
    seed_legacy_zero_row(&storage, &mine, &my_zero).await;
    seed_legacy_zero_row(&storage, &theirs, &their_zero).await;

    let report = storage
        .repair_unindexable_vectors(&mine, "chunks", false)
        .await
        .expect("repair must succeed");
    assert_eq!(
        report.scanned, 1,
        "the scan must not even LOOK at the other tenant's rows — a repair that \
         counted 2 here would have been reading across the partition"
    );
    assert_eq!(report.repaired, 1);

    // The other tenant's damage is the other tenant's to schedule. An operator
    // repairing one scope must not silently mutate every other scope on the box.
    let mut typed = storage.client().typed();
    let theirs_vec: Option<Vec<u8>> = typed
        .hget(row_key(&theirs, &their_zero).as_bytes(), "vec")
        .await
        .expect("HGET vec must succeed");
    assert_eq!(
        theirs_vec.map(|v| v.len()),
        Some(DIM * 4),
        "the other scope's row must be untouched"
    );
}
