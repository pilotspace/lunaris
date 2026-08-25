//! F40 — `WorkingMemory::read(k)` must be an EXACT-KEY read, not a ranked one.
//!
//! ## The defect
//!
//! `read` ran a fused Vector + BM25 `top(8)` retrieval and took the first hit.
//! With no usable embedder the vector leg carries no signal, and on a key whose
//! text the FT analyzer reduces to nothing the BM25 leg does not carry it
//! either — so `read` answered `Ok(None)` for a value that was just written.
//! **`None` means "not found", so the caller could not tell a missing key from
//! an unusable index.**
//!
//! Measured before the fix, against a live Moon with an explicit
//! `NoopEmbedder`: `pad.read("note.md")` → `Ok(None)` immediately after
//! `pad.write("note.md", "hello v2")`.
//!
//! ## Why the embedder is injected rather than env-configured
//!
//! `LUNARIS_EMBEDDER_GGUF` is read once per process into a `OnceLock`, so a
//! test that sets it races every sibling in the same binary — including
//! siblings that never name it, because they reach it through the code under
//! test. `Lunaris::open_with_embedder` takes the dependency as an argument,
//! which is both race-free and exact: this test asserts a property of the READ,
//! and injecting the embedder is what isolates the read from the environment.
//!
//! ## Why this one can go red
//!
//! The suite that previously covered write→read
//! (`coding_session_memory_v2_delegation_round_trip`) is gated on
//! `embedder_available`, so in the one configuration that reproduces F40 it
//! SKIPS — and a skip is a pass. This test needs no staged model at all: the
//! Noop embedder is the fixture.

use std::sync::Arc;

use lunaris::{CodingSessionMemory, Lunaris};
use lunaris_core::{Embedder, NoopEmbedder, Scope};

/// Open against the Moon under test, or announce a skip.
///
/// Routed through the shared strict-skip helper so a job that promised a Moon
/// fails instead of reporting success for a suite that ran nothing (F27).
fn moon_url() -> Option<String> {
    match std::env::var("MOON_URL").ok().filter(|s| !s.trim().is_empty()) {
        Some(u) => Some(u),
        None => {
            lunaris_test_harness::strict_skip::note_unavailable(
                "MOON_URL unset — working_memory_exact_key_read needs a live Moon",
            );
            None
        }
    }
}

#[tokio::test]
async fn an_exact_key_read_does_not_need_an_embedder() {
    let Some(url) = moon_url() else { return };

    // The whole point: NO usable embedder. Every vector is zeros.
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let lunaris =
        Arc::new(Lunaris::open_with_embedder(&url, embedder).await.expect("open_with_embedder"));
    let session = format!("f40-exact-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), Scope::dev(), &session);

    pad.write("note.md", "hello v2").await.expect("write");

    let got = pad.read("note.md").await.expect("read must not error");
    assert_eq!(
        got.as_deref(),
        Some("hello v2"),
        "read() answered {got:?} for a key written moments earlier on the same handle. \
         With a Noop embedder the ranked path cannot resolve the key, and `None` is \
         indistinguishable from 'never written' — F40. An exact-key read must not depend \
         on the quality of an embedding."
    );
}

#[tokio::test]
async fn the_exact_key_read_still_distinguishes_a_key_that_is_absent() {
    // The anti-overcorrection arm. A fix that returned the most recent write
    // for ANY key, or that stopped answering `None` at all, would satisfy the
    // test above and be worse than the defect. Same handle, same scope, same
    // Noop embedder — only the key differs.
    let Some(url) = moon_url() else { return };

    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let lunaris =
        Arc::new(Lunaris::open_with_embedder(&url, embedder).await.expect("open_with_embedder"));
    let session = format!("f40-absent-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), Scope::dev(), &session);

    pad.write("present.md", "i am here").await.expect("write");

    let present = pad.read("present.md").await.expect("read present");
    assert_eq!(present.as_deref(), Some("i am here"), "fixture check: the write must land");

    let absent = pad.read("never-written.md").await.expect("read absent");
    assert_eq!(
        absent, None,
        "a key that was never written must still read as None — got {absent:?}. \
         Both arms run against the SAME pad, so this cannot pass by reading an empty scope."
    );
}

#[tokio::test]
async fn two_sessions_writing_the_same_key_do_not_alias() {
    // A secondary index is a new way to collide. `source_index_key` hashes the
    // FULLY-QUALIFIED source (`{session_prefix}{path}`), not the path — but a
    // hash of the wrong input is a valid key that reads back the wrong value,
    // and both arms would look green if each test used its own session and read
    // only its own key.
    //
    // So both pads live in the SAME scope, use the SAME key name, and each read
    // asserts the value the OTHER pad did not write. An exclusion-only check
    // ("A is not B") would pass if a read went to an empty partition; asserting
    // the positive value on both sides cannot.
    let Some(url) = moon_url() else { return };

    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let lunaris =
        Arc::new(Lunaris::open_with_embedder(&url, embedder).await.expect("open_with_embedder"));
    let run = ulid::Ulid::new();
    let a = CodingSessionMemory::new(lunaris.clone(), Scope::dev(), &format!("f40-a-{run}"));
    let b = CodingSessionMemory::new(lunaris.clone(), Scope::dev(), &format!("f40-b-{run}"));

    a.write("shared.md", "from A").await.expect("write a");
    b.write("shared.md", "from B").await.expect("write b");

    assert_eq!(
        a.read("shared.md").await.expect("read a").as_deref(),
        Some("from A"),
        "session A read the wrong value for a key session B also wrote"
    );
    assert_eq!(
        b.read("shared.md").await.expect("read b").as_deref(),
        Some("from B"),
        "session B read the wrong value for a key session A also wrote"
    );
}

#[tokio::test]
async fn a_rewrite_reads_back_the_newer_value() {
    // The index is last-writer-wins, which is what `read`'s contract already
    // says ("the latest content at `path`"). Pinned because an index that kept
    // the FIRST episode id would satisfy every other test in this file — each
    // of those writes a key exactly once.
    let Some(url) = moon_url() else { return };

    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let lunaris =
        Arc::new(Lunaris::open_with_embedder(&url, embedder).await.expect("open_with_embedder"));
    let session = format!("f40-rewrite-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), Scope::dev(), &session);

    pad.write("note.md", "first").await.expect("write 1");
    assert_eq!(pad.read("note.md").await.expect("read 1").as_deref(), Some("first"));

    pad.write("note.md", "second").await.expect("write 2");
    assert_eq!(
        pad.read("note.md").await.expect("read 2").as_deref(),
        Some("second"),
        "a rewrite must be visible — the source index is last-writer-wins"
    );
}
