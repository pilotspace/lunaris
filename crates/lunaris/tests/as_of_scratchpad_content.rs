//! F42 — the scratchpad's time-travel read must return the content that was
//! WRITTEN, and only the version that is current.
//!
//! ## What was actually wrong, measured on a live Moon
//!
//! `read_at` — shared by [`AsOfScratchpad::read`] and the multi-chunk fallback
//! inside `CodingSessionMemory::read` — concatenated `Hit::text` across every
//! hit. Writing one value and reading it back through the pad's own time-travel
//! API produced:
//!
//! ```text
//! written : "line one -- with \"ascii quotes\" and ... ellipsis\nline two"
//! read()  : "line one -- with \"ascii quotes\" and ... ellipsis\nline two"   (exact)
//! as_of() : "“ line one – with \"ascii quotes \" and … ellipsis\\nline two ”"
//! ```
//!
//! ### Defect 1 — the content is not the content
//!
//! `--` became an en dash, `...` became `…`, the outer ASCII quotes became
//! typographic quotes, spaces appeared inside the value, and the `\n` escape
//! survived as a literal backslash-n.
//!
//! **The mechanism is the CHUNKER, not the search index.** This was recorded
//! as "`read_at` concatenates the ANALYZER-NORMALISED index text"; that is
//! wrong, and it matters, because it points at the wrong component. `Hit::text`
//! is hydrated from the *chunk KV payload* (`hydrate.rs`), so the index is not
//! involved. The chunker parses with `pulldown_cmark::Options::all()` — which
//! includes `ENABLE_SMART_PUNCTUATION` — and rebuilds chunk text by
//! concatenating `Event::Text`, so the rewrite happens at INGEST and is baked
//! into the stored chunk. Measured directly:
//!
//! ```text
//! chunk_markdown("\"AAA_ONLY_NOTES\"") -> ["“ AAA_ONLY_NOTES ”"]   (1 draft)
//! ```
//!
//! Chunk text is therefore a lossy projection of the document for every
//! caller, on every surface. `WorkingMemory::read` already knows this — its
//! doc comment says so and it recovers from the parent Episode instead. That
//! is why `CodingSessionMemory::read` round-trips exactly and only the
//! `read_at` paths do not.
//!
//! ### Defect 2 — superseded versions are glued onto the answer
//!
//! Recorded as "every chunk appears twice, so the reconstruction is doubled".
//! It is not a doubling. Every write mints a NEW Episode (`write_inner`), all
//! versions stay indexed under the same `source`, `Filter::Eq` matches all of
//! them, and `read_at` concatenates every hit. Measured with one write plus
//! one edit:
//!
//! ```text
//! as_of() : "“ …v2 content… ”“ FIRST_VERSION_BODY ”"
//! ```
//!
//! So the answer accumulates stale content in proportion to how often the path
//! was edited. A single-chunk value never "doubles"; a twice-written one leaks
//! its previous body.
//!
//! ### Not a defect — historical pins are already honest
//!
//! An `as_of` more than an hour back is refused end-to-end with
//! `StorageError::NotSupported` ("Moon KV read_as_of/scan_range cannot answer a
//! historical snapshot"), which is the 0.6.2 task-9 guard working as designed.
//! `AsOfScratchpad::read` on Moon therefore only ever serves LIVE-WINDOW pins,
//! and the fix below cannot regress the historical case: recovering from the
//! episode KV row hits exactly the same guard.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::{CodingSessionMemory, Lunaris};
use lunaris_core::Scope;

/// Punctuation the markdown chunker's smart-punctuation pass rewrites, plus a
/// JSON escape — every character class the measurement above showed mangled.
const CONTENT: &str = "line one -- with \"ascii quotes\" and ... ellipsis\nline two";

fn moon_url() -> Option<String> {
    match std::env::var("MOON_URL").ok().filter(|s| !s.trim().is_empty()) {
        Some(u) => Some(u),
        None => {
            lunaris_test_harness::strict_skip::note_unavailable(
                "MOON_URL unset — as_of_scratchpad_content needs a live Moon",
            );
            None
        }
    }
}

async fn pad_in(url: &str, tag: &str) -> (Arc<Lunaris>, CodingSessionMemory) {
    let scope = Scope::new(format!("f42{tag}{}", ulid::Ulid::new())).expect("scope");
    let lunaris = Arc::new(Lunaris::open(url).await.expect("open"));
    let session = format!("f42-{tag}-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), scope, &session);
    (lunaris, pad)
}

/// The time-travel read must be byte-for-byte what was written.
///
/// `CodingSessionMemory::read` already round-trips exactly (it recovers from
/// the Episode payload), so this is not an unreachable standard — it is the
/// same contract the sibling method already meets on the same data.
#[tokio::test]
async fn the_as_of_read_returns_the_content_that_was_written() {
    let Some(url) = moon_url() else { return };
    let (lunaris, pad) = pad_in(&url, "exact").await;

    pad.write("notes", CONTENT).await.expect("write");

    // Anti-vacuity: the live read must already be exact, or this test is
    // measuring a broken write rather than a broken read.
    let live = pad.read("notes").await.expect("live read").expect("live read must find it");
    assert_eq!(live, CONTENT, "the LIVE read must be exact before the as-of read can be judged");

    let ts = lunaris.clock().tick();
    let got = pad
        .as_of(ts)
        .read("notes")
        .await
        .expect("as-of read must not error")
        .expect("as-of read of a just-written path must not be None");

    assert_eq!(
        got, CONTENT,
        "the as-of read returned a rewritten copy of the value. Chunk text is a LOSSY \
         projection — the chunker parses with pulldown_cmark::Options::all(), which enables \
         ENABLE_SMART_PUNCTUATION, and rebuilds text from the event stream, so `--` becomes an \
         en dash and ASCII quotes become typographic ones AT INGEST. Content must be recovered \
         from the parent Episode payload, the way `WorkingMemory::read` already does."
    );
}

/// A path that has been edited must read back as its CURRENT version only.
#[tokio::test]
async fn the_as_of_read_does_not_glue_on_superseded_versions() {
    let Some(url) = moon_url() else { return };
    let (lunaris, pad) = pad_in(&url, "mvcc").await;

    pad.write("notes", "FIRST_VERSION_BODY").await.expect("write v1");
    pad.edit("notes", "FIRST_VERSION_BODY", CONTENT).await.expect("edit to v2");

    let ts = lunaris.clock().tick();
    let got = pad
        .as_of(ts)
        .read("notes")
        .await
        .expect("as-of read must not error")
        .expect("as-of read of an edited path must not be None");

    assert!(
        !got.contains("FIRST_VERSION_BODY"),
        "the as-of read glued the SUPERSEDED version onto the current one: {got:?}. Every \
         write mints a new Episode under the same `source`; `Filter::Eq` matches all of them \
         and `read_at` concatenated every hit, so the answer accumulates stale content in \
         proportion to how often the path was edited."
    );
    assert_eq!(got, CONTENT, "the as-of read must return exactly the current version");
}

/// Anti-overcorrection: resolving to a single version must not turn a value
/// too large for one chunk into a truncated one. This is the case the
/// concatenation existed for, and it must survive the fix.
#[tokio::test]
async fn a_multi_chunk_value_still_reads_back_whole() {
    let Some(url) = moon_url() else { return };
    let (lunaris, pad) = pad_in(&url, "big").await;

    // Comfortably past the 500-token chunk target, with a unique marker at each
    // end so a truncation at either boundary is visible rather than plausible.
    let mut big = String::from("HEAD_MARKER_F42\n");
    for i in 0..400 {
        big.push_str(&format!("paragraph {i} with enough words to carry real token weight\n\n"));
    }
    big.push_str("TAIL_MARKER_F42");

    pad.write("bigdocument", &big).await.expect("write big");

    let ts = lunaris.clock().tick();
    let got = pad
        .as_of(ts)
        .read("bigdocument")
        .await
        .expect("as-of read must not error")
        .expect("as-of read of a large path must not be None");

    assert!(got.contains("HEAD_MARKER_F42"), "lost the head of a multi-chunk value");
    assert!(
        got.contains("TAIL_MARKER_F42"),
        "lost the TAIL of a multi-chunk value — a single-version fix must not become a \
         single-CHUNK fix: {} bytes returned for {} written",
        got.len(),
        big.len()
    );
    assert_eq!(got, big, "a multi-chunk value must round-trip whole and exact");
}

/// Defect 3 — a path whose NAME analyses to an empty FT query makes the whole
/// read error out, rather than returning content.
///
/// Isolated, not inferred: the multi-chunk test above first failed with
/// `ERR empty query after analysis`; renaming the path from `"big"` to
/// `"bigdocument"` — same content, same size — removed the error and left only
/// the mangling failure. So the trigger is the path NAME, not the payload.
///
/// `read_at` passes the path as the recall query text and has no retry arm for
/// an unusable FT query. `WorkingMemory::find` has had one since the
/// stopword-key fix ("stopword-like keys such as `state` were write-OK /
/// read-IMPOSSIBLE"), which is exactly this failure — `read_at` simply never
/// received it. That makes short or stopword-like paths (`big`, `state`, `up`)
/// writable but unreadable through the time-travel API.
#[tokio::test]
async fn a_stopword_like_path_is_readable_not_an_error() {
    let Some(url) = moon_url() else { return };
    let (lunaris, pad) = pad_in(&url, "stop").await;

    pad.write("big", "SHORT_NAME_BODY").await.expect("write must succeed");

    // Anti-vacuity: the write really is retrievable by the sibling API, so a
    // failure below is the READ path, not a lost write.
    let live = pad.read("big").await.expect("live read").expect("live read must find it");
    assert_eq!(live, "SHORT_NAME_BODY");

    let ts = lunaris.clock().tick();
    let got = pad.as_of(ts).read("big").await.expect(
        "as-of read of a stopword-like path must not ERROR. `read_at` uses the path as the \
         recall query text with no `is_ft_query_unusable` retry, so Moon's analyzer reduces \
         it to an empty query and the read fails outright — write-OK, read-IMPOSSIBLE.",
    );
    assert_eq!(got.as_deref(), Some("SHORT_NAME_BODY"));
}
