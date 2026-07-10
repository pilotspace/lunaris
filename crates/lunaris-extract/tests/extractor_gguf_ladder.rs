//! "Built ≠ wired" proof for the extractor Q4 GGUF ladder.
//!
//! Constructing a passing unit test against `QuantizedCandleBackend` in
//! isolation (lunaris-llm) proves the backend works; it does NOT prove the
//! production entry point (`lunaris_extract::CandleGemma3_4B::new`, the
//! exact function `lunaris::handle::default_extractor()` calls) actually
//! routes to it. This test drives the real production constructor with
//! `LUNARIS_EXTRACTOR_GGUF` set and NO F32 weights staged at the default
//! cache path — if `resolve_backend`'s GGUF branch were dead code (e.g. a
//! `cfg` typo, an env-var name drift, or the ladder silently falling
//! through to F32), `CandleGemma3_4B::new` would hard-fail here with the
//! F32 path's "weights missing" error, because the F32 `model.safetensors`
//! genuinely is not staged on this host (see the workstream context doc's
//! baseline caveat). Success is therefore direct evidence the quantized
//! branch executed, not an artifact of a lucky fallback.
//!
//! Gated behind `extractor-gguf`. Skipped (not failed) when the real
//! artifacts aren't staged.
//!
//! ```bash
//! export LUNARIS_EXTRACTOR_GGUF=/Volumes/Games/tindang-repo/models/gemma-3-4b-it-qat-q4_0/gemma-3-4b-it-q4_0.gguf
//! export LUNARIS_EXTRACTOR_DIR=/Volumes/Games/tindang-repo/models/gemma-3-4b-it-hf
//! cargo test -p lunaris-extract --release --features extractor-gguf \
//!     --test extractor_gguf_ladder -- --nocapture
//! ```

#![cfg(feature = "extractor-gguf")]

use std::path::PathBuf;

use candle_core::Device;
use lunaris_extract::{CandleGemma3_4B, CandleGemma3_4BOpts, ChunkInput, Extractor};
use ulid::Ulid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candle_gemma3_4b_new_routes_to_quantized_backend_when_gguf_env_set() {
    if std::env::var_os("LUNARIS_EXTRACTOR_GGUF").is_none() {
        eprintln!(
            "[skip] LUNARIS_EXTRACTOR_GGUF unset; see module docs to run this test for real."
        );
        return;
    }
    let gguf = PathBuf::from(std::env::var_os("LUNARIS_EXTRACTOR_GGUF").unwrap());
    if !gguf.exists() {
        eprintln!("[skip] LUNARIS_EXTRACTOR_GGUF set but not found at {}", gguf.display());
        return;
    }
    match std::env::var_os("LUNARIS_EXTRACTOR_DIR").map(PathBuf::from) {
        Some(dir) if dir.join("tokenizer.json").exists() && dir.join("config.json").exists() => {}
        Some(dir) => {
            eprintln!(
                "[skip] LUNARIS_EXTRACTOR_DIR={} missing tokenizer.json/config.json",
                dir.display()
            );
            return;
        }
        None => {
            eprintln!(
                "[skip] LUNARIS_EXTRACTOR_DIR unset; see module docs to run this test for real."
            );
            return;
        }
    }

    // Point `model_path` at a directory that definitely does NOT contain
    // F32 weights — proves the quantized branch, not a lucky F32 hit at the
    // default cache dir.
    let no_f32_dir =
        std::env::temp_dir().join(format!("lunaris_test_no_f32_weights_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&no_f32_dir);

    let opts = CandleGemma3_4BOpts {
        model_path: Some(no_f32_dir.clone()),
        device: Device::Cpu,
        // The 150ms/450ms production defaults are unwinnable for CPU Q4
        // decode of a 4B model (design doc §3's own admission) — generous
        // overrides here so the real decode isn't swallowed by the
        // fail-open batch-timeout fallback (which would return an empty,
        // but successful, RawExtractionBatch and defeat the point of this
        // test).
        batch_timeout_ms: 120_000,
        per_chunk_timeout_ms: 120_000,
        max_new_tokens: 256,
    };

    let t0 = std::time::Instant::now();
    let extractor = CandleGemma3_4B::new(opts)
        .await
        .expect("CandleGemma3_4B::new must succeed via the quantized branch (no F32 weights exist at model_path)");
    eprintln!("[it] CandleGemma3_4B::new took {}ms", t0.elapsed().as_millis());

    let chunk = ChunkInput {
        chunk_id: Ulid::new(),
        text: "Alice met Bob in Paris.".to_string(),
        heading_path: vec![],
    };
    let t1 = std::time::Instant::now();
    let batch = extractor.extract(Ulid::new(), std::slice::from_ref(&chunk)).await.expect(
        "extract must not error (fail-open semantics — empty extraction on timeout, never Err)",
    );
    eprintln!("[it] extract took {}ms; batch = {batch:?}", t1.elapsed().as_millis());

    assert_eq!(batch.by_chunk.len(), 1, "one chunk in, one RawExtraction out");
    assert_eq!(batch.by_chunk[0].source_chunk_id, chunk.chunk_id);

    let _ = std::fs::remove_dir_all(&no_f32_dir);
}
