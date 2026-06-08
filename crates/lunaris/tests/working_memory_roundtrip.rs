//! Regression — `WorkingMemory::read` / `grep` MUST recover the VERBATIM
//! stored value from the authoritative Episode `content`, NOT from the lossy
//! markdown chunk `text`.
//!
//! ## Why this is a correctness contract, not a nicety
//!
//! `WorkingMemory::write` stores the value as `serde_json::to_string(&v)` on
//! the Episode `content` field, then rides `Lunaris::ingest` → the markdown
//! chunker. The chunker runs `pulldown_cmark` with `ENABLE_SMART_PUNCTUATION`,
//! which rewrites straight quotes (`"` → `"` `"`), `--` → en-dash, and `...`
//! → ellipsis in the chunk `text`. Reconstructing a JSON object/array value
//! from `Hit::text` therefore yields invalid JSON and corrupts the value.
//! Recovering from the Episode `content` (which is never chunked) is lossless.
//!
//! ## What this test pins
//!
//! 1. An object value packed with smart-punctuation triggers round-trips
//!    byte-identically through `write` → `read`.
//! 2. `grep` recovers verbatim values for every key under a sub-prefix, and
//!    does not leak keys from a sibling sub-prefix.
//!
//! Both assertions run on the embedded (`memory://`) backend — the
//! `lunaris-mcp` default — whose `keyword_search` returns `NotSupported`. So
//! the test also proves `WorkingMemory`'s INTERNAL vector-only fallback fires
//! (no second fallback path bolted on by callers).

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris::{Lunaris, WorkingMemory};
use lunaris_core::{Scope, StubEmbedder};
use serde_json::json;

async fn working_memory(scope_name: &str) -> WorkingMemory {
    let embedder = Arc::new(StubEmbedder::new(768));
    let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
    let scope = Scope::new(scope_name).unwrap();
    WorkingMemory::new(Arc::new(lunaris), scope, "scratchpad/")
}

#[tokio::test]
async fn read_recovers_verbatim_object_value() {
    let wm = working_memory("wm-rt-read").await;

    // Straight quotes, `--`, and `...` all trip ENABLE_SMART_PUNCTUATION — so
    // recovering from the chunk `text` would corrupt this value.
    let value = json!({
        "answer": 42,
        "note": "use \"quotes\", em -- dashes, and ... ellipses",
        "tags": ["a", "b"],
        "nested": { "ok": true }
    });
    wm.write("rt-key", value.clone()).await.unwrap();

    let got = wm.read("rt-key").await.unwrap();
    assert_eq!(
        got,
        Some(value),
        "read must return the byte-identical stored value (recovered from Episode content)"
    );
}

#[tokio::test]
async fn grep_recovers_verbatim_values_and_respects_sub_prefix() {
    let wm = working_memory("wm-rt-grep").await;

    let v0 = json!({ "i": 0, "s": "q \"0\" -- x" });
    let v1 = json!({ "i": 1, "s": "q \"1\" -- y" });
    wm.write("note-0", v0.clone()).await.unwrap();
    wm.write("note-1", v1.clone()).await.unwrap();
    // A key under a DIFFERENT sub-prefix must NOT match "note-".
    wm.write("other-9", json!({ "i": 9 })).await.unwrap();

    let mut got = wm.grep("note-").await.unwrap();
    got.sort_by(|a, b| a.0.cmp(&b.0));

    let values: Vec<serde_json::Value> = got.iter().map(|(_, v)| v.clone()).collect();
    assert_eq!(
        values,
        vec![v0, v1],
        "grep must recover verbatim values for every key under the sub-prefix"
    );
    assert!(
        got.iter().all(|(src, _)| src.starts_with("scratchpad/note-")),
        "grep must only return keys under the queried sub-prefix; got {got:?}"
    );
}

/// A single scratchpad value large enough to chunk-split MUST grep to exactly
/// ONE entry — one per key, not one per chunk.
///
/// `write` stores the value as a single-line `serde_json::to_string`, but the
/// chunker walks that paragraph word-by-word and emits a chunk once accumulated
/// tokens cross `target_tokens = 500` (the surrogate counter is
/// `ceil(words * 1.3)`, so ≥ ~385 whitespace words spill into a second chunk).
/// Every resulting chunk shares the parent Episode's `episode_id`. Without
/// dedup, `grep` recovers the identical value once per chunk and emits N
/// duplicate `(source, value)` pairs — which also crowd the `top_k` window and
/// can drop distinct sibling keys. This pins one-entry-per-key.
#[tokio::test]
async fn grep_dedups_a_chunk_split_value_to_one_entry() {
    let wm = working_memory("wm-rt-grep-split").await;

    // ~800 whitespace-separated words → > 500 surrogate tokens → ≥ 2 chunks.
    let big = (0..800).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
    let value = json!({ "notes": big });
    wm.write("big-note", value.clone()).await.unwrap();

    let got = wm.grep("big-note").await.unwrap();
    assert_eq!(
        got.len(),
        1,
        "a chunk-split value must yield exactly one grep entry (per key, not per chunk); got {} entries",
        got.len()
    );
    assert_eq!(got[0].0, "scratchpad/big-note");
    assert_eq!(got[0].1, value, "the single entry must carry the verbatim value");
}
