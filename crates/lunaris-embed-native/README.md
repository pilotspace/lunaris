# `lunaris-embed-native`

Pure-Rust native [ModernBERT][modernbert] embedder for
[`ibm-granite/granite-embedding-311m-multilingual-r2`][granite-r2],
built on `candle-core` / `candle-nn` / `candle-transformers`.

This is the v0.4 milestone embedder — purpose-built for a single
model. It replaces the retired fastembed/candle-gemma/ollama trio
(deleted in N-03) as the sole default backend.

## What this crate does

Given an `Arc<dyn lunaris_core::Embedder>` constructed via
`NativeEmbedder::open(opts)`, you get a 768-d unit-norm sentence
embedder that matches the reference
`sentence_transformers.SentenceTransformer("ibm-granite/granite-embedding-311m-multilingual-r2")`
**to within f32 epsilon** on a 100-prompt multilingual panel (35 EN +
35 VI + 30 code):

| Metric                 | Result      | Gate          |
|------------------------|-------------|---------------|
| Mean cosine drift      | 1.5 × 10⁻⁸  | ≤ 5 × 10⁻³    |
| Max  cosine drift      | 1.2 × 10⁻⁷  | ≤ 2 × 10⁻²    |
| Cross-prompt baseline  | 0.7386 dot  | (non-vacuity) |

The mean and max drifts exceed their gates by **five orders of
magnitude** — bit-for-bit equivalence at f32 precision.

## Pipeline

Inside `NativeEmbedder::embed_batch`:

```
tokenizers::Tokenizer
    → (input_ids, attention_mask) tensors
candle_transformers::models::modernbert::ModernBert::forward(..)
    → (batch, seq_len, 768) hidden states (encoder-only, no causal mask)
CLS-pool — take position 0 along seq                      # 1_Pooling
    → (batch, 768)
cast → fp32; L2-normalize (eps = 1e-12)                   # 2_Normalize
    → unit-norm (batch, 768) rows
```

The forward pass itself is `ModernBert` from candle-transformers
0.10.x; this crate wires it up with:

- weight-key rename (`model.<x>` → `<x>` — the granite-r2 safetensors
  store keys without the `model.` prefix that candle's loader expects),
- CLS-pool + L2-normalize as the post-processing,
- `tokio::task::spawn_blocking` boundary so the synchronous candle
  forward doesn't stall the async runtime,
- safe `VarBuilder::from_buffered_safetensors` load path
  (`#![forbid(unsafe_code)]`).

## Running the numerical-equivalence integration test

The test (`tests/numerical_equivalence.rs`) is gated behind the
`embedder-it` feature flag, mirroring `lunaris-embed`'s convention.

```bash
# 1) Download granite-r2 + generate the 100-prompt reference fixture
python3 scripts/spike-generate-reference-embeddings.py
# → ~/.cache/lunaris/spike/granite-r2/models--ibm-granite--…/
# → crates/lunaris-embed-native/tests/fixtures/reference_embeddings.json
#   (committed to the repo, 1.64 MB)

# 2) Point env vars at the cached snapshot dir
SNAP=~/.cache/lunaris/spike/granite-r2/models--ibm-granite--granite-embedding-311m-multilingual-r2/snapshots/<rev>
export GRANITE_R2_WEIGHTS_PATH="$SNAP/model.safetensors"
export GRANITE_R2_TOKENIZER_PATH="$SNAP/tokenizer.json"
export GRANITE_R2_CONFIG_PATH="$SNAP/config.json"

# 3) Run the IT in release mode (CPU fp32; ~4.5s for 100 prompts)
cargo test -p lunaris-embed-native --features embedder-it --release \
  --test numerical_equivalence -- --nocapture
```

If any of the three `GRANITE_R2_*_PATH` env vars is unset, the test
skips with a notice (does not fail). Default `cargo test` runs the
offline unit suite only.

## API surface

```rust
use std::sync::Arc;
use std::path::PathBuf;
use candle_core::Device;
use lunaris_core::Embedder;
use lunaris_embed_native::{NativeEmbedder, NativeEmbedderOpts, GRANITE_R2_DIM};

# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
let embedder = NativeEmbedder::open(NativeEmbedderOpts {
    weights_path:   PathBuf::from("/path/to/model.safetensors"),
    tokenizer_path: PathBuf::from("/path/to/tokenizer.json"),
    config_path:    PathBuf::from("/path/to/config.json"),
    device:         Device::Cpu,
})?;
assert_eq!(embedder.dim(), GRANITE_R2_DIM);   // 768

let vecs = embedder.embed_batch(&["hello", "xin chào"]).await?;
let _: Arc<dyn Embedder> = Arc::new(embedder); // dyn-compatible
# Ok(()) }
```

## Q4_K_M quantized path (feature `embedder-gguf`)

> **Status (2026-05-14): scaffold landed; forward-pass body pending.** The
> GGUF conversion gate is cleared (artifact verified end-to-end), the
> SHA-256 is pinned in `lib.rs::GRANITE_R2_GGUF_Q4_SHA256`, and the RED
> integration test under `tests/quantized_equivalence.rs` is in place.
> `NativeQuantizedEmbedder::embed_batch` returns
> `NotImplemented` until the next-session port lands the actual quantized
> forward pass — see `.planning/phases/N-01-step-2-quantized-gguf/SUMMARY.md`
> for the handoff.

```bash
# 1) one-shot conversion: granite-r2 safetensors → Q4_K_M GGUF (idempotent)
bash scripts/spike-convert-granite-r2-to-gguf.sh

# 2) export env vars (the FP16 paths + the new GGUF path)
export GRANITE_R2_WEIGHTS_PATH=~/.cache/lunaris/spike/granite-r2/models--ibm-granite--granite-embedding-311m-multilingual-r2/snapshots/dba7b0ee9d789f330fecfb85df57699f9e7d9c42/model.safetensors
export GRANITE_R2_TOKENIZER_PATH=~/.cache/lunaris/spike/granite-r2/models--ibm-granite--granite-embedding-311m-multilingual-r2/snapshots/dba7b0ee9d789f330fecfb85df57699f9e7d9c42/tokenizer.json
export GRANITE_R2_CONFIG_PATH=~/.cache/lunaris/spike/granite-r2/models--ibm-granite--granite-embedding-311m-multilingual-r2/snapshots/dba7b0ee9d789f330fecfb85df57699f9e7d9c42/config.json
export GRANITE_R2_GGUF_PATH=~/.cache/lunaris/spike/granite-r2/gguf/granite-r2-311m-Q4_K_M.gguf

# 3) drift gate (RED until the port lands)
cargo test -p lunaris-embed-native \
    --features 'embedder-it,embedder-gguf' \
    --test quantized_equivalence -- --nocapture
```

Pinned artifact:

| Field         | Value                                                                |
|---------------|----------------------------------------------------------------------|
| Path          | `~/.cache/lunaris/spike/granite-r2/gguf/granite-r2-311m-Q4_K_M.gguf` |
| Size          | **240.7 MiB** (61.1 % smaller than the 620 MB fp16 safetensors)      |
| SHA-256       | `0768a38b0bc9900e89bb15ae0b6ea2ca7db130759e0eca226119610aedf5e276`   |
| BPW           | 6.10 (Q4_K with Q5_0 / Q6_K / Q8_0 fallbacks per tensor)             |
| llama.cpp HEAD| `ccb9e9b7c` (PR [#22716] applied for granite-r2 tokenizer support)   |

Drift gate (asserted by `tests/quantized_equivalence.rs` once the port
lands): **mean cosine drift ≤ 0.01 / max ≤ 0.03** versus the FP16 native
path, with a non-vacuity guard on cross-prompt cosine < 0.97.

[#22716]: https://github.com/ggml-org/llama.cpp/pull/22716

## Out of scope

Even after both FP16 and Q4 land here, the following stay in follow-ups:

- Reranker port (`granite-reranker` or `bge-reranker-base`)
- Metal / CUDA quantized paths (the hardware-optimization milestone)
- Lunaris handle wiring (`Lunaris::open(...).with_native_embedder(...)`)
- Metal / CUDA paths (the hardware-optimization milestone)

## References

- [ModernBERT paper (arXiv 2412.13663)][modernbert]
- [granite-embedding-311m-multilingual-r2 model card][granite-r2]
- `candle_transformers::models::modernbert` — upstream Rust
  reimplementation that this crate composes with.
- `sentence_transformers.SentenceTransformer` — the reference pipeline
  whose output we match.

## Unicode normalization contract

Inputs are **NFC-normalized before tokenization** (see
`src/tokenizer.rs::encode_batch`). Callers passing NFD-encoded text
(iOS / macOS clipboards routinely emit NFD) get the same embedding as
the equivalent NFC-encoded text — `"Tiếng Việt"` in NFC and in NFD
produce bit-identical token streams and therefore bit-identical
embeddings.

**NFKC is not applied.** NFKC collapses compatibility characters
(full-width ↔ half-width Latin, ligatures, super/subscripts) which
changes semantics for code/identifier embeddings. NFC only re-composes
canonically-equivalent sequences, which preserves all distinctions that
matter to retrieval.

The Python reference fixture
(`scripts/spike-generate-reference-embeddings.py`) applies the same
NFC pre-pass before calling `sentence-transformers`, so the
numerical-equivalence drift gate stays at f32 epsilon (mean ≈ 1.5e-8,
max ≈ 1.2e-7 as of `schema_version: 2`).

History: pre-v0.2.2 the tokenizer skipped NFC normalization; NFC vs NFD
of the same user-visible string produced cosines of 0.83–0.93 instead
of 1.0 — see P1-1 in
`.planning/phases/N-01-step-1-modernbert-fp16/P1-VERIFICATION-RESULT.md`.

## License

Apache-2.0 OR MIT — same as the rest of the Lunaris workspace.
The granite-r2 weights themselves are Apache-2.0 (IBM).

[modernbert]: https://arxiv.org/abs/2412.13663
[granite-r2]:
    https://huggingface.co/ibm-granite/granite-embedding-311m-multilingual-r2
