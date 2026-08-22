//! F21 (boundary half) — `Filter::ValidTimeRange` is documented half-open
//! `[after, before)` and every implementation treats it as CLOSED.
//!
//! Gated behind `moon-it`. To run:
//!
//! ```bash
//! MOON_TEST_BINARY=/path/to/moon \
//!   cargo test -p lunaris-storage-moon --features moon-it \
//!   --test valid_time_half_open -- --nocapture
//! ```
//!
//! ## The defect
//!
//! `lunaris_core::storage::types::Filter::ValidTimeRange` says, verbatim:
//! "Constrain hits to items whose `valid_time` falls within the half-open
//! range `[after, before)`". Three implementations disagree with it:
//!
//! * `vector.rs::render_knn_filter` emits `@valid_time:[lo hi]`, and Moon's
//!   numeric-range parser reads a bare bound as INCLUSIVE (it accepts a
//!   `(`-prefix for exclusive — see `vendor/moon/src/text/query/parse.rs`);
//! * `keyword.rs::filter_to_moon` emits the same closed range;
//! * `vector.rs::filter_matches`, the client-side post-filter, tests
//!   `ms <= b.wall_ms`.
//!
//! So an item sitting exactly ON the upper bound comes back, and
//! `TemporalQuery::between(lo, hi)` returns one row too many at every window
//! whose edge lands on an event.
//!
//! ## Why it was invisible until now
//!
//! Nothing could reach this bound. Until F21's main fix the valid axis was
//! the INGEST instant for every caller of every API, so a historical window
//! matched zero rows and an off-by-one at its edge could not show. It
//! surfaced the moment the axis started carrying real dates: the TypeScript
//! parity suite went from `expected 0 to be 6` straight to `expected 7 to be
//! 6` — the 2025-01-16 event leaking into `[2025-01-10, 2025-01-16)`.
//!
//! ## What these tests assert
//!
//! Behaviour against a live Moon, on both retrieval legs. The `lo` boundary
//! is asserted alongside the `hi` boundary in the same test, so "the upper
//! bound excludes" cannot be satisfied by a filter that has quietly become
//! too narrow at both ends.

#![cfg(feature = "moon-it")]

use lunaris_core::storage::keyword::KeywordPort;
use lunaris_core::storage::types::Filter;
use lunaris_core::{Hlc, Scope, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;
use lunaris_test_harness::EphemeralMoon;

mod common;

const DIM: usize = 768;

/// Millisecond instants around a window. `LO` and `HI` are the bounds
/// themselves; the other two sit one millisecond outside and inside.
const BEFORE_LO: u64 = 1_736_467_199_999;
const LO: u64 = 1_736_467_200_000; // 2025-01-10T00:00:00Z
const INSIDE: u64 = 1_736_899_200_000; // 2025-01-15T00:00:00Z
const HI: u64 = 1_736_985_600_000; // 2025-01-16T00:00:00Z

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

fn upsert(id: &[u8], valid_time_ms: u64, text: &str) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "chunks".into(),
        id: id.to_vec(),
        embedding: unit_vector(),
        metadata: serde_json::json!({ "text": text, "valid_time_ms": valid_time_ms }),
    }
}

struct Fixture {
    before_lo: Vec<u8>,
    at_lo: Vec<u8>,
    inside: Vec<u8>,
    at_hi: Vec<u8>,
}

async fn seed(storage: &MoonStorage, scope: &Scope) -> Fixture {
    let f = Fixture {
        before_lo: ulid::Ulid::new().to_bytes().to_vec(),
        at_lo: ulid::Ulid::new().to_bytes().to_vec(),
        inside: ulid::Ulid::new().to_bytes().to_vec(),
        at_hi: ulid::Ulid::new().to_bytes().to_vec(),
    };
    storage
        .atomic_write(
            scope,
            &[
                upsert(&f.before_lo, BEFORE_LO, "timeline event outside below"),
                upsert(&f.at_lo, LO, "timeline event on the lower bound"),
                upsert(&f.inside, INSIDE, "timeline event inside the window"),
                upsert(&f.at_hi, HI, "timeline event on the upper bound"),
            ],
        )
        .await
        .expect("seed write must succeed");
    f
}

fn window() -> Filter {
    Filter::ValidTimeRange {
        after: Some(Hlc::from_parts(LO, 0, 0)),
        before: Some(Hlc::from_parts(HI, 0, 0)),
    }
}

fn assert_half_open(hits: &[Vec<u8>], f: &Fixture, leg: &str) {
    let has = |id: &Vec<u8>| hits.iter().any(|h| h == id);

    // Vacuity floor first: if the window matched nothing at all, every
    // "must be absent" assertion below would pass for the wrong reason.
    assert!(
        has(&f.inside),
        "{leg}: the row squarely INSIDE the window is missing, so this test proves nothing \
         about either boundary. got {} hits",
        hits.len()
    );
    assert!(
        has(&f.at_lo),
        "{leg}: the lower bound must be INCLUSIVE — `[after, ...`. A filter that excludes it \
         is too narrow at both ends, which would make the upper-bound assertion below pass \
         for the wrong reason. got {} hits",
        hits.len()
    );
    assert!(
        !has(&f.before_lo),
        "{leg}: a row one millisecond BELOW the window came back. got {} hits",
        hits.len()
    );
    assert!(
        !has(&f.at_hi),
        "{leg}: F21 — a row sitting exactly ON the upper bound came back. \
         `Filter::ValidTimeRange` is documented half-open `[after, before)`, so `hi` itself is \
         OUT. Moon reads a bare numeric bound as inclusive; the render needs a `(`-prefix on \
         the upper bound, and the client-side post-filter needs `<` rather than `<=`. \
         got {} hits",
        hits.len()
    );
}

/// The KNN leg — `vector_search` with a `ValidTimeRange`, which Moon
/// evaluates server-side via `@valid_time:[...]`.
#[tokio::test]
async fn vector_search_treats_the_upper_bound_as_exclusive() {
    let Some(moon) = private_moon("vector_search_treats_the_upper_bound_as_exclusive").await else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f21-vec-{}", ulid::Ulid::new())).expect("scope name valid");
    let f = seed(&storage, &scope).await;

    let hits = storage
        .vector_search(&scope, "chunks", &unit_vector(), 50, Some(&window()), None, false)
        .await
        .expect("vector_search must succeed");
    let ids: Vec<Vec<u8>> = hits.into_iter().map(|h| h.id).collect();
    assert_half_open(&ids, &f, "vector_search");
}

/// The BM25 leg — a separate render path (`keyword.rs::filter_to_moon`) with
/// its own copy of the range syntax, so it needs its own assertion.
#[tokio::test]
async fn keyword_search_treats_the_upper_bound_as_exclusive() {
    let Some(moon) = private_moon("keyword_search_treats_the_upper_bound_as_exclusive").await
    else {
        return;
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope = Scope::new(format!("f21-kw-{}", ulid::Ulid::new())).expect("scope name valid");
    let f = seed(&storage, &scope).await;

    let hits = storage
        .keyword_search(&scope, "chunks", "timeline", 50, Some(&window()), None)
        .await
        .expect("keyword_search must succeed");
    let ids: Vec<Vec<u8>> = hits.into_iter().map(|h| h.id).collect();
    assert_half_open(&ids, &f, "keyword_search");
}
