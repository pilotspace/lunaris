//! F26 (residual half) — a KNN prefilter Moon cannot parse returns MORE rows,
//! not an error.
//!
//! ## The hazard
//!
//! Moon has two numeric-range parsers and they disagree. The full FT.SEARCH
//! grammar handles `(`-prefixed exclusive bounds; the KNN prefilter
//! (`{expr}=>[KNN k @vec $q]`) is parsed by a separate, smaller one at
//! `vendor/moon/src/command/vector_search/ft_search/parse.rs`. Every branch of
//! that parser is written `... .parse().ok()?` — a `?` on the WHOLE filter. One
//! token it cannot read and `parse_filter_string` returns `None`, whereupon the
//! caller runs an UNFILTERED search. No error, no warning, no metric.
//!
//! A filter that has silently stopped filtering is indistinguishable from one
//! that legitimately matched everything. On a scoped store that is one schema
//! change away from a confidentiality bug rather than a recall bug. Filed
//! upstream as pilotspace/moon#648; `vendor/moon` is read-only here, so this
//! file is the Lunaris-side containment.
//!
//! ## What F21's test already covers, and what it does not
//!
//! `valid_time_half_open.rs` pins the ONE filter shape that was known broken:
//! it would catch a dropped `ValidTimeRange`, because the rows outside the
//! window would come back. It says nothing about the other shapes
//! `render_knn_filter` emits, and nothing about the next one anyone adds.
//!
//! That is the gap this file closes. Every filter `render_knn_filter` returns
//! `Some(_)` for is asserted to have ACTUALLY constrained a live Moon — by
//! exact row set, against an unfiltered baseline seeded in the same scope. A
//! filter that degrades to a pass-through returns the baseline, which is
//! exactly what these assertions reject.
//!
//! The `And` case is the load-bearing one. Its conjuncts are joined with a
//! space and handed to that parser as a single string, so a composition bug
//! takes out both halves at once — and each conjunct alone still matches more
//! rows than the pair does, so a too-large answer cannot be mistaken for a
//! correct one.

#![cfg(feature = "moon-it")]

use lunaris_core::storage::types::Filter;
use lunaris_core::{Hlc, Scope, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;
use lunaris_test_harness::EphemeralMoon;

mod common;

const DIM: usize = 768;

/// Four rows on two axes, arranged so no single predicate isolates a row:
/// `source` splits them 2/2 and the time window splits them 2/2 the other
/// way, so the AND of the two is a strict subset of either.
const T_EARLY: u64 = 1_736_467_200_000; // 2025-01-10T00:00:00Z
const T_LATE: u64 = 1_736_985_600_000; // 2025-01-16T00:00:00Z

async fn private_moon(test: &str) -> Option<EphemeralMoon> {
    match EphemeralMoon::spawn().await {
        Ok(m) => Some(m),
        Err(e) => {
            common::note_moon_unreachable(format!("{test}: no ephemeral Moon ({e})"));
            None
        }
    }
}

fn unit_vector() -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[0] = 1.0;
    v
}

fn upsert(id: &[u8], source: &str, valid_time_ms: u64) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "chunks".into(),
        id: id.to_vec(),
        embedding: unit_vector(),
        metadata: serde_json::json!({
            "text": "prefilter corpus row",
            "source": source,
            "valid_time_ms": valid_time_ms,
        }),
    }
}

/// Named by the two axes: `<source>_<when>`.
struct Fixture {
    notes_early: Vec<u8>,
    notes_late: Vec<u8>,
    other_early: Vec<u8>,
    other_late: Vec<u8>,
}

impl Fixture {
    fn all(&self) -> Vec<Vec<u8>> {
        vec![
            self.notes_early.clone(),
            self.notes_late.clone(),
            self.other_early.clone(),
            self.other_late.clone(),
        ]
    }
}

async fn seed(storage: &MoonStorage, scope: &Scope) -> Fixture {
    let f = Fixture {
        notes_early: ulid::Ulid::new().to_bytes().to_vec(),
        notes_late: ulid::Ulid::new().to_bytes().to_vec(),
        other_early: ulid::Ulid::new().to_bytes().to_vec(),
        other_late: ulid::Ulid::new().to_bytes().to_vec(),
    };
    storage
        .atomic_write(
            scope,
            &[
                upsert(&f.notes_early, "notes.md", T_EARLY),
                upsert(&f.notes_late, "notes.md", T_LATE),
                upsert(&f.other_early, "other.md", T_EARLY),
                upsert(&f.other_late, "other.md", T_LATE),
            ],
        )
        .await
        .expect("seed write must succeed");
    f
}

async fn search(storage: &MoonStorage, scope: &Scope, filter: Option<&Filter>) -> Vec<Vec<u8>> {
    let hits = storage
        .vector_search(scope, "chunks", &unit_vector(), 50, filter, None, false)
        .await
        .expect("vector_search must succeed");
    let mut ids: Vec<Vec<u8>> = hits.into_iter().map(|h| h.id).collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    ids.sort();
    ids
}

/// The window `[T_EARLY, T_LATE)` — `T_LATE` itself is OUT (half-open).
fn early_only() -> Filter {
    Filter::ValidTimeRange {
        after: Some(Hlc::from_parts(T_EARLY, 0, 0)),
        before: Some(Hlc::from_parts(T_LATE, 0, 0)),
    }
}

fn notes_only() -> Filter {
    Filter::Eq { field: "source".into(), value: serde_json::json!("notes.md") }
}

/// The vacuity floor for every case below. If the corpus does not come back
/// whole with no filter, "the filter narrowed the result" would be satisfied
/// by a Moon that simply lost the rows.
#[tokio::test]
async fn the_unfiltered_corpus_comes_back_whole() {
    let Some(moon) = private_moon("the_unfiltered_corpus_comes_back_whole").await else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f26-base-{}", ulid::Ulid::new())).expect("scope name valid");
    let f = seed(&storage, &scope).await;

    assert_eq!(
        search(&storage, &scope, None).await,
        sorted(f.all()),
        "the 4-row corpus must come back whole with no filter — every assertion in this file \
         reads a SMALLER set as proof the filter applied, which proves nothing if the rows \
         were never there"
    );
}

/// `Filter::Eq` on `source` renders to `@source:{notes.md}`, which the KNN
/// prefilter parser reads as a TAG equality.
#[tokio::test]
async fn a_source_prefilter_actually_reaches_moon() {
    let Some(moon) = private_moon("a_source_prefilter_actually_reaches_moon").await else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f26-src-{}", ulid::Ulid::new())).expect("scope name valid");
    let f = seed(&storage, &scope).await;

    assert_eq!(
        search(&storage, &scope, Some(&notes_only())).await,
        sorted(vec![f.notes_early.clone(), f.notes_late.clone()]),
        "a `source` prefilter returned something other than the two notes.md rows. Four rows \
         means the filter was DROPPED, not narrowed: Moon's KNN prefilter parser returns None \
         for the whole expression on any token it cannot read, and the caller then runs an \
         unfiltered search. See F26 / pilotspace/moon#648"
    );
}

/// `Filter::ValidTimeRange` renders to `@valid_time:[lo hi-1]`. The `hi-1` is
/// the F26 workaround for the `(`-bound the prefilter parser rejects; this
/// asserts the workaround is still in place and still exact.
#[tokio::test]
async fn a_valid_time_prefilter_actually_reaches_moon() {
    let Some(moon) = private_moon("a_valid_time_prefilter_actually_reaches_moon").await else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f26-vt-{}", ulid::Ulid::new())).expect("scope name valid");
    let f = seed(&storage, &scope).await;

    assert_eq!(
        search(&storage, &scope, Some(&early_only())).await,
        sorted(vec![f.notes_early.clone(), f.other_early.clone()]),
        "a `valid_time` prefilter returned something other than the two T_EARLY rows. Four rows \
         means the range was dropped entirely — which is what a `(`-prefixed bound does here, \
         silently. See F26 / pilotspace/moon#648"
    );
}

/// The composition case. `Filter::And` joins its conjuncts with a space into
/// ONE string for that parser, so a rendering mistake in either half takes out
/// both. The expected answer is a single row — strictly smaller than what
/// either conjunct returns alone — so a half-applied filter is also caught.
#[tokio::test]
async fn an_and_prefilter_applies_both_conjuncts_not_one() {
    let Some(moon) = private_moon("an_and_prefilter_applies_both_conjuncts_not_one").await else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f26-and-{}", ulid::Ulid::new())).expect("scope name valid");
    let f = seed(&storage, &scope).await;

    let both = Filter::And(vec![notes_only(), early_only()]);
    let got = search(&storage, &scope, Some(&both)).await;

    assert_eq!(
        got,
        sorted(vec![f.notes_early.clone()]),
        "an AND prefilter must apply BOTH conjuncts. Two rows means only one half survived the \
         render; four means the whole expression failed to parse and Moon fell through to an \
         unfiltered search. Neither is reported as an error anywhere. See F26"
    );
}
