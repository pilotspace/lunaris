//! W0.4 — `lunaris try` end to end, from the stranger's side.
//!
//! The whole value of a trial path is that it produces a REAL recall with
//! nothing installed. So this test does what a stranger does: it runs the
//! shipped binary with one argument and asserts that hits from the built-in
//! sample corpus appear on stdout. Nothing is stubbed except the embedder
//! weights (see below).
//!
//! ## What is real here and what is not
//!
//! Real: the in-process Moon (`launch_embedded_moon`), the storage handshake,
//! `lunaris_memory_service::protocol::dispatch` for every ingest AND for the
//! recall, the FT index, the hydrate, and the rendering.
//!
//! Not real: the embedding vectors. `LUNARIS_TRY_EMBEDDER=stub` swaps
//! `lunaris_core::StubEmbedder` (deterministic 768-d hashes) in for the
//! granite-r2 GGUF. That is a deliberate, documented seam and it exists
//! because this box allows exactly ONE llama.cpp process machine-wide —
//! concurrent Metal contexts deadlock. The seam is narrow on purpose: it
//! changes the `Arc<dyn Embedder>` and nothing else, so every line of plumbing
//! under test is the line that ships. What it CANNOT prove is that granite
//! produces semantically useful neighbours; `tests/stage_contract.rs` pins the
//! artifact identity instead, and the ordering quality is what the LongMemEval
//! harness measures.
//!
//! ## Feature gate
//!
//! `embedded-moon` is never in a default feature set (CLAUDE.md), so this file
//! compiles to nothing in a plain `cargo test --workspace`. Run it with:
//!
//! ```text
//! cargo test -p lunaris-cli --features embedded-moon --test try_end_to_end
//! ```

#![cfg(feature = "embedded-moon")]

use std::process::Command;

/// A phrase that appears verbatim in exactly one built-in sample memory. If it
/// shows up in the rendered hits, the bytes made the full round trip:
/// ingest → chunk → embed → Moon write → FT search → hydrate → render.
const CORPUS_MARKER: &str = "single-shard";

fn run_try(dir: &tempfile::TempDir, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lunaris"));
    cmd.arg("try");
    cmd.args(extra);
    // The trial's data dir. Never ~/.lunaris in a test.
    cmd.env("LUNARIS_TRY_DIR", dir.path());
    // Deterministic hash vectors instead of llama.cpp — see the module docs.
    cmd.env("LUNARIS_TRY_EMBEDDER", "stub");
    // Belt and braces: even if the seam above regressed, point the GGUF
    // resolver at nothing so no inference can start by accident.
    cmd.env("LUNARIS_EMBEDDER_GGUF", "/nonexistent/try-e2e.gguf");
    // A stray HOME would let discovery find the developer's own store.
    cmd.env("HOME", dir.path());
    cmd.output().expect("spawn the lunaris binary")
}

#[test]
fn try_brings_up_its_own_store_and_prints_real_hits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = run_try(&dir, &[]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "`lunaris try` must exit 0 on a clean machine.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    assert!(
        stdout.contains(CORPUS_MARKER),
        "`lunaris try` must print hits carrying the built-in corpus text. A trial \
         that reports success without recalling anything is the exact failure \
         mode this command exists to disprove.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    assert!(
        !stdout.contains("no hits"),
        "the sample corpus must always produce at least one hit\n{stdout}"
    );
}

/// A stranger's second command is almost always "ask it something else". If
/// `--query` did not reach the recall, the trial would be a fixed demo reel
/// rather than a store you can talk to.
#[test]
fn try_answers_a_caller_supplied_query() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = run_try(&dir, &["--query", "what did we decide about sharding"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("what did we decide about sharding"),
        "the rendered output must echo the query that was actually run — a trial \
         that silently answers a different question is worse than one that \
         fails\n{stdout}"
    );
    assert!(stdout.contains(CORPUS_MARKER), "stdout:\n{stdout}");
}

/// Re-running must be idempotent: the corpus is dedupe-keyed, so a second run
/// against the same durable dir must not double the store or change the
/// answer's shape. This is what makes `~/.lunaris/try` safe to keep.
#[test]
fn a_second_run_reuses_the_durable_store_without_duplicating_the_corpus() {
    let dir = tempfile::tempdir().expect("tempdir");

    let first = run_try(&dir, &[]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));

    let second = run_try(&dir, &[]);
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert!(stdout.contains(CORPUS_MARKER), "stdout:\n{stdout}");
    assert!(
        stdout.contains("6 sample memories"),
        "the corpus size must be stable across runs — a growing count means the \
         dedupe key is not doing its job and the durable dir would rot\n{stdout}"
    );
}
