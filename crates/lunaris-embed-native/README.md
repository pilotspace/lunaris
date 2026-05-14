# `lunaris-embed-native`

Pure-Rust native [ModernBERT][modernbert] embedder for
[`ibm-granite/granite-embedding-311m-multilingual-r2`][granite-r2],
built on `candle-core` / `candle-nn` / `candle-transformers`.

This is the v0.4 milestone embedder — purpose-built for a single
model so we can replace the fastembed/candle-gemma/ollama trio
in `lunaris-embed` with one well-understood backend.

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

## Out of scope

This crate is **only** the FP16 forward pass. Follow-up milestones:

- Q4 quantization (8-bit / 4-bit weights, target ~310 MB RSS)
- Reranker port (`granite-reranker` or `bge-reranker-base`)
- Deletion of fastembed / candle-gemma / ollama from `lunaris-embed`
- Lunaris handle wiring (`Lunaris::open(...).with_native_embedder(...)`)

## References

- [ModernBERT paper (arXiv 2412.13663)][modernbert]
- [granite-embedding-311m-multilingual-r2 model card][granite-r2]
- `candle_transformers::models::modernbert` — upstream Rust
  reimplementation that this crate composes with.
- `sentence_transformers.SentenceTransformer` — the reference pipeline
  whose output we match.

## License

Apache-2.0 OR MIT — same as the rest of the Lunaris workspace.
The granite-r2 weights themselves are Apache-2.0 (IBM).

[modernbert]: https://arxiv.org/abs/2412.13663
[granite-r2]:
    https://huggingface.co/ibm-granite/granite-embedding-311m-multilingual-r2
