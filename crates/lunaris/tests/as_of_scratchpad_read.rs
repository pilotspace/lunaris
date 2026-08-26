//! F41 — `AsOfScratchpad::read` must read the pad's OWN scope, and must not
//! return a sibling path's content.
//!
//! ## The defects, both measured before the fix
//!
//! `read_at` — shared by [`AsOfScratchpad::read`] and the multi-chunk fallback
//! inside `CodingSessionMemory::read` — recalls through the BARE `Arc<Lunaris>`
//! handle. A bare handle's `RetrievalBuilder` carries no scope and defaults to
//! `Scope::dev()`, while the pad writes through `WorkingMemory` into
//! `self.scope`. The two never meet, so the time-travel read answered `None`
//! for every caller that passed a real scope — which is every caller, since
//! `CodingSessionMemory::new` takes the scope as a separate argument
//! precisely because the handle is not expected to be pre-scoped.
//!
//! The discriminating measurement, on a live Moon: the same writes and the
//! same read, differing ONLY in the pad's scope.
//!
//! ```text
//! real scope        -> None
//! Scope::dev()      -> Some("… BBB_DEV_NOTES … YYY_DEV_OLD …")
//! ```
//!
//! The `dev` arm is what exposed the second defect. `read_at` filters with
//! `Filter::StartsWith { field: "source", prefix: source }` and concatenates
//! every hit, so `read("notes")` also matches `notes-old` — the prefix of a
//! path is not the path. Its own doc-comment claims "exact source equality";
//! `Filter::Eq` is what delivers that, and unlike `StartsWith` it renders as a
//! real Moon KNN prefilter (`@source:{v}`) instead of being post-hydrate
//! filtered.
//!
//! ## Why `contains` and not `assert_eq`
//!
//! When this file was written, `read_at` returned a rewritten, doubled-looking
//! string, and the cause was recorded here as "the SEARCH INDEX's text, which
//! is normalised". **That diagnosis was wrong on both counts** and is corrected
//! in `as_of_scratchpad_content.rs` (F42), which measured it: `Hit::text` is
//! hydrated from the chunk KV payload, so the index was never involved — the
//! rewrite is the CHUNKER's `pulldown_cmark::Options::all()`
//! (`ENABLE_SMART_PUNCTUATION`) applied at ingest; and the repetition was not
//! chunks appearing twice but SUPERSEDED VERSIONS being concatenated onto the
//! current one.
//!
//! This file still asserts with `contains` rather than `assert_eq`, because
//! what it fixes is the SCOPE and the FILTER, not the content — keeping it
//! insensitive to the content defects means F42's fix cannot accidentally be
//! what makes these tests pass.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::{CodingSessionMemory, Lunaris};
use lunaris_core::Scope;

/// Open against the Moon under test, or announce a skip through the shared
/// strict-skip helper so a job that promised a Moon fails rather than
/// reporting success for a suite that ran nothing (F27).
fn moon_url() -> Option<String> {
    match std::env::var("MOON_URL").ok().filter(|s| !s.trim().is_empty()) {
        Some(u) => Some(u),
        None => {
            lunaris_test_harness::strict_skip::note_unavailable(
                "MOON_URL unset — as_of_scratchpad_read needs a live Moon",
            );
            None
        }
    }
}

async fn pad_in(url: &str, scope: Scope, tag: &str) -> (Arc<Lunaris>, CodingSessionMemory) {
    let lunaris = Arc::new(Lunaris::open(url).await.expect("open"));
    let session = format!("f41-{tag}-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), scope, &session);
    (lunaris, pad)
}

#[tokio::test]
async fn the_as_of_read_uses_the_pads_own_scope() {
    let Some(url) = moon_url() else { return };

    // A REAL scope — what every production caller passes.
    let scope = Scope::new(format!("f41scope{}", ulid::Ulid::new())).expect("scope");
    let (lunaris, pad) = pad_in(&url, scope, "scope").await;

    pad.write("notes", "AAA_ONLY_NOTES").await.expect("write");

    let ts = lunaris.clock().tick();
    let got = pad.as_of(ts).read("notes").await.expect("as-of read must not error");

    let text = got.unwrap_or_else(|| {
        panic!(
            "as_of(..).read(\"notes\") answered None for a value written moments earlier on \
             the same pad. `read_at` recalls through the BARE handle, whose builder defaults \
             to Scope::dev(), while the write went to the pad's own scope — so the public \
             time-travel API has never returned anything for a caller with a real scope (F41)."
        )
    });
    assert!(
        text.contains("AAA_ONLY_NOTES"),
        "as-of read returned {text:?}, which does not contain the value that was written"
    );
}

#[tokio::test]
async fn the_as_of_read_does_not_return_a_sibling_path() {
    let Some(url) = moon_url() else { return };

    let scope = Scope::new(format!("f41sib{}", ulid::Ulid::new())).expect("scope");
    let (lunaris, pad) = pad_in(&url, scope, "sib").await;

    // `notes-old` HAS `notes` as a prefix. That is the whole point.
    pad.write("notes", "AAA_ONLY_NOTES").await.expect("write notes");
    pad.write("notes-old", "ZZZ_OLD_CONTENT").await.expect("write notes-old");

    let ts = lunaris.clock().tick();
    let text = pad
        .as_of(ts)
        .read("notes")
        .await
        .expect("as-of read must not error")
        .expect("as-of read of a written path must not be None");

    assert!(
        text.contains("AAA_ONLY_NOTES"),
        "as-of read of \"notes\" lost its own content: {text:?}"
    );
    assert!(
        !text.contains("ZZZ_OLD_CONTENT"),
        "as-of read of \"notes\" also returned the content of \"notes-old\": {text:?}. \
         `read_at` filters with Filter::StartsWith and concatenates every hit, so any path \
         that EXTENDS the requested one is folded into the answer — the prefix of a path is \
         not the path. Its own doc-comment says \"exact source equality\" (F41)."
    );
}

/// Anti-overcorrection: switching the filter to exact equality must not break
/// the case the prefix filter was there for. A pad reads back a path it wrote
/// even when a sibling with a SHARED PREFIX IN THE OTHER DIRECTION exists —
/// i.e. the requested path extends an existing one.
#[tokio::test]
async fn an_exact_filter_still_finds_a_path_that_extends_another() {
    let Some(url) = moon_url() else { return };

    let scope = Scope::new(format!("f41ext{}", ulid::Ulid::new())).expect("scope");
    let (lunaris, pad) = pad_in(&url, scope, "ext").await;

    pad.write("notes", "AAA_SHORT").await.expect("write notes");
    pad.write("notes-old", "ZZZ_LONG").await.expect("write notes-old");

    let ts = lunaris.clock().tick();
    let text = pad
        .as_of(ts)
        .read("notes-old")
        .await
        .expect("as-of read must not error")
        .expect("as-of read of the LONGER path must not be None");

    assert!(
        text.contains("ZZZ_LONG"),
        "as-of read of \"notes-old\" did not return its own content: {text:?}"
    );
    assert!(
        !text.contains("AAA_SHORT"),
        "as-of read of \"notes-old\" also returned \"notes\": {text:?}"
    );
}
