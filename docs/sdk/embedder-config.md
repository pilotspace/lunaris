# SDK embedder + reranker configuration

Customize the embedder and reranker Lunaris uses for ingest + recall — without
touching environment variables — via the `EmbedderConfig` and `RerankerConfig`
SDK types. This guide covers both the Python (`pip install lunaris`) and
TypeScript (`npm i @pilotspace/lunaris`) SDKs in lockstep; every code sample
appears in both languages.

> **v0.4 native default (N-03 cutover, 2026-05-14).** Lunaris ships an
> in-process **candle-native** embedder + reranker as the supported default.
> The pre-v0.4 fastembed / ONNX-Runtime / production-Ollama-embedder paths
> were deleted — see
> [`docs/migration/0.3-to-0.4-native-default.md`](../migration/0.3-to-0.4-native-default.md).
> If you are migrating off `EmbedderConfig.fastembed()` /
> `.from_onnx_bytes()` / `.ollama()`, start there.

## Overview

`EmbedderConfig` is an opaque handle that wraps a resolved `Arc<dyn Embedder>`
on the Rust side. Construct it via one of three factory methods, then hand it
to `lunaris.open(url, embedder=...)` (Python) or `.withEmbedder(cfg)`
(TypeScript) to override the env-driven default. `RerankerConfig` mirrors the
same shape for the cross-encoder rerank pass.

Use the SDK surface when:

- You want to pin the **quantized GGUF** variant from code (RSS-constrained
  host) rather than rebuilding the wheel / `.node` per environment.
- You're running Lunaris in a multi-tenant process where each tenant gets a
  different model directory.
- You pre-stage weights for an **air-gapped** host and want to point at the
  model directory from code.

Use env vars (`LUNARIS_EMBEDDER_DIR`, `LUNARIS_RERANKER_DIR`, …) when you want
one process-wide default with no code changes — both surfaces co-exist, with
the SDK overriding env at handle-construction time.

> **FFI cliff.** You cannot implement the Rust `Embedder` or `Reranker` trait
> from Python or TypeScript. Per-call FFI callbacks would be too slow for the
> hot retrieval path. Roll-your-own backends are a Rust-crate-only escape
> hatch. See [§Limits](#limits) for the full statement.

---

## The customization paths

Each path appears as a `#[staticmethod]` (Python) or `#[napi(factory)]`
(TypeScript) on `EmbedderConfig`. The TS sibling uses camelCase opts bags; the
Python sibling uses keyword arguments. Naming is otherwise identical.

### Path 1 — Native (default)

Most users land here. `native()` loads the in-process
**`ibm-granite/granite-embedding-311m-multilingual-r2`** model (Apache-2.0,
ModernBERT, 768-d, FP16) via candle. On first call it auto-downloads the
weights into `~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/`
(~620 MiB safetensors, one-time per host). **No Ollama, no ONNX Runtime, no
external service required.**

**Python**

```python
import asyncio
import lunaris
from lunaris import EmbedderConfig

async def main():
    cfg = EmbedderConfig.native()                 # default cache dir
    # or pin a pre-staged directory (air-gapped hosts):
    # cfg = EmbedderConfig.native(model_dir="/models/granite-r2")
    mem = await lunaris.open("moon://127.0.0.1:6380", embedder=cfg)
    # ... ingest / recall as usual

asyncio.run(main())
```

**TypeScript**

```typescript
import { open, EmbedderConfig } from "@pilotspace/lunaris";

const cfg = EmbedderConfig.native();              // default cache dir
// or: EmbedderConfig.native({ modelDir: "/models/granite-r2" });
const mem = (await open("moon://127.0.0.1:6380")).withEmbedder(cfg);
// ... ingest / recall as usual
```

The TypeScript SDK uses a chainable `withEmbedder` / `withReranker` extension
on the opened handle — each call returns a **new** handle carrying the
override; the original is unaffected. The Python SDK uses
`open(url, embedder=..., reranker=...)` kwargs. Both produce the same runtime
effect — a Rust handle whose `Arc<dyn Embedder>` carries the SDK-supplied
backend.

### Path 2 — Native quantized (GGUF)

Same granite-r2 model, served from a **Q4_K_M GGUF** (~240 MiB) for
RSS-constrained hosts. Requires the wheel / `.node` to be built with
`--features embedder-gguf`; without it the factory raises.

**Python**

```python
cfg = EmbedderConfig.native_quantized(
    gguf_path="/models/granite-r2-311m-Q4_K_M.gguf",
    model_dir=None,                # optional: dir holding tokenizer.json + config
)
mem = await lunaris.open(url, embedder=cfg)
```

**TypeScript**

```typescript
const cfg = EmbedderConfig.nativeQuantized({
  ggufPath: "/models/granite-r2-311m-Q4_K_M.gguf",
  // modelDir: "/models/granite-r2",   // optional
});
const mem = (await open(url)).withEmbedder(cfg);
```

The GGUF carries the quantized weights; the tokenizer + config are read from
`model_dir` (defaults to the same cache layout as `native()`).

### Path 3 — Noop

Deterministic zero-vector embedder for tests and offline / air-gapped CI where
you don't want to load real weights. The vector dimension is configurable so it
matches your index width.

**Python**

```python
cfg = EmbedderConfig.noop(dim=768)        # default 768; must match your index
mem = await lunaris.open(url, embedder=cfg)
```

**TypeScript**

```typescript
const cfg = EmbedderConfig.noop(768);     // default 768
const mem = (await open(url)).withEmbedder(cfg);
```

`noop` produces no semantic signal — recall degenerates to keyword / graph
retrieval. Use it only for plumbing tests, never production.

### Path 4 — Air-gapped Ollama escape hatch (no SDK factory)

There is **no `EmbedderConfig.ollama()` SDK factory** in v0.4+. The Ollama HTTP
embedder is an **operator-only escape hatch**, selected at build + env time,
not from SDK code:

- Build with `--features embed-remote` on the `lunaris-memory` umbrella.
- Set `LUNARIS_EMBEDDER_OLLAMA_URL` (e.g. `http://localhost:11434`); the
  resolver then constructs `lunaris_embed_remote::OllamaEmbedder` instead of
  the native default.
- Optionally set `LUNARIS_OLLAMA_MODEL` (default `embeddinggemma:300m`).

This is **not the supported path** — it exists for air-gapped hosts that
already run an Ollama server. Prefer Path 1 (native) or Path 2 (GGUF).

### Pairing with a `RerankerConfig`

`RerankerConfig` mirrors the embedder surface: `native()` loads
**`BAAI/bge-reranker-v2-m3`** (Apache-2.0, XLM-RoBERTa cross-encoder, FP32,
sigmoid output ∈ [0,1]); `native_quantized(gguf_path)` serves the
Q5_K_M-imatrix GGUF (requires `--features reranker-gguf`); `noop()` skips the
cross-encoder rescoring pass entirely.

**Python**

```python
from lunaris import EmbedderConfig, RerankerConfig

mem = await lunaris.open(
    url,
    embedder=EmbedderConfig.native(),
    reranker=RerankerConfig.native(),
)

# Cheap fallback — disables the cross-encoder rescoring pass:
mem = await lunaris.open(url, embedder=EmbedderConfig.native(), reranker=RerankerConfig.noop())
```

**TypeScript**

```typescript
import { open, EmbedderConfig, RerankerConfig } from "@pilotspace/lunaris";

const mem = (await open(url))
  .withEmbedder(EmbedderConfig.native())
  .withReranker(RerankerConfig.native());

// Cheap fallback — disables the cross-encoder rescoring pass:
const fallback = (await open(url))
  .withEmbedder(EmbedderConfig.native())
  .withReranker(RerankerConfig.noop());
```

---

## Configuration reference

### `EmbedderConfig.native`

| Argument | Python type | TS field | Default | Notes |
|---|---|---|---|---|
| `model_dir` / `modelDir` | `Path \| str \| None` | `string?` | `$LUNARIS_EMBEDDER_DIR` → `~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/` | Directory holding the granite-r2 safetensors + `tokenizer.json`. Auto-downloaded on first use if absent. |

### `EmbedderConfig.native_quantized` / `EmbedderConfig.nativeQuantized`

| Argument | Python type | TS field | Required | Notes |
|---|---|---|---|---|
| `gguf_path` / `ggufPath` | `Path \| str` | `string` | yes | Path to the granite-r2 Q4_K_M GGUF. Requires `--features embedder-gguf`. |
| `model_dir` / `modelDir` | `Path \| str \| None` | `string?` | no | Dir holding `tokenizer.json` + `config.json`; defaults to the `native()` cache layout. |

### `EmbedderConfig.noop`

| Argument | Python type | TS field | Default |
|---|---|---|---|
| `dim` | `int` | `number?` | `768` |

Returns a deterministic zero-vector embedder. The `dim` MUST match the width
your storage index was created with.

### `RerankerConfig.native`

| Argument | Python type | TS field | Default | Notes |
|---|---|---|---|---|
| `model_dir` / `modelDir` | `Path \| str \| None` | `string?` | `$LUNARIS_RERANKER_DIR` → `~/.cache/lunaris/models/bge-reranker-v2-m3/` | Directory holding the bge-reranker-v2-m3 weights + tokenizer. |

### `RerankerConfig.native_quantized` / `RerankerConfig.nativeQuantized`

| Argument | Python type | TS field | Required | Notes |
|---|---|---|---|---|
| `gguf_path` / `ggufPath` | `Path \| str` | `string` | yes | Path to the bge-reranker Q5_K_M-imatrix GGUF. Requires `--features reranker-gguf`. |
| `model_dir` / `modelDir` | `Path \| str \| None` | `string?` | no | Dir holding the tokenizer; defaults to the `native()` cache layout. |

### `RerankerConfig.noop`

No arguments. Returns a [`NoopReranker`] passthrough; `Hit.rerank_applied`
will be `False` for results that flow through this reranker so callers can
detect the degraded path.

---

## Operational notes

- **First-call HF Hub fetch.** `native()` pulls weights from Hugging Face Hub
  on first use. Ensure `~/.cache/lunaris/models/` (or the dir named by
  `LUNARIS_EMBEDDER_DIR` / `LUNARIS_RERANKER_DIR`) has write permission and
  ~1 GB free. The download is one-time per host; subsequent processes reuse the
  cache.
- **Cache layout.** Default location is `~/.cache/lunaris/models/` with one
  subdirectory per model (`granite-embedding-311m-multilingual-r2/`,
  `bge-reranker-v2-m3/`). Multiple Lunaris processes can safely share it on the
  same host.
- **Env vars that influence the default.** `LUNARIS_EMBEDDER_DIR`,
  `LUNARIS_RERANKER_DIR`, `LUNARIS_EMBEDDER_GGUF`, `LUNARIS_RERANKER_GGUF`
  (the latter two require the matching `*-gguf` feature), and
  `LUNARIS_EMBEDDER_OLLAMA_URL` (requires `--features embed-remote`). SDK-supplied
  values always override env; env overrides nothing (it's the bottom of the
  resolution chain). **`LUNARIS_EMBEDDER_BACKEND` / `LUNARIS_RERANKER_BACKEND`
  are retired** — there is no env-var backend swap in v0.4+.
- **Accelerator features.** The native crates are `Device`-parameterized;
  build `lunaris-embed-native` / `lunaris-rerank-native` with
  `cpu-accelerate` (macOS BLAS), `cpu-mkl` (Intel BLAS), `metal` (Apple
  Silicon GPU), `cuda`, or `cuda-fa2` to activate the matching candle compute
  path. The CPU FP16 path is always available with no extra feature.
- **Cross-SDK consistency.** Vectors produced by `EmbedderConfig.native()` are
  byte-identical across the Python and TypeScript SDKs for the same input
  (enforced mechanically — see [§Next steps](#next-steps)).

---

## Limits

1. **FFI cliff — no custom trait impls from Python or TypeScript.** The Rust
   `Embedder` and `Reranker` traits require an async fn that returns
   `Vec<Vec<f32>>`. Implementing them from Python / TS would require per-call
   FFI callbacks, which are too slow for the retrieval hot path. The factory
   methods on `EmbedderConfig` / `RerankerConfig` cover every customization the
   Rust crate supports MINUS the "roll your own trait impl" escape hatch, which
   is Rust-only. If you need a backend that doesn't fit the native / GGUF /
   noop shape, contribute a constructor to `lunaris-embed-native` and we'll
   surface it through both SDKs.

2. **`noop` dim MUST match your index.** Lunaris's storage backends (Moon
   `FT.SEARCH`, Postgres pgvector) are pre-sized for a fixed embedding width at
   index-create time. `EmbedderConfig.noop(dim=...)` lets you pick any width,
   so a mismatch against an existing index will corrupt recall — pass the same
   `dim` (768 for granite-r2) the index was built with.

3. **Quantized variants require a feature flag.** `native_quantized` /
   `nativeQuantized` only work when the artifact was built with
   `--features embedder-gguf` (embedder) / `--features reranker-gguf`
   (reranker). Without it the factory raises at config time rather than
   silently falling back to FP16.

4. **Weights load in-process.** `native()` / `native_quantized()` load
   safetensors / GGUF into candle in the same process as your application.
   Source weights from a trusted location; treat them as you would any binary
   loaded into your process.

5. **The Python `open(url, embedder=...)` kwarg vs. the TypeScript
   `.withEmbedder(cfg)` chain.** These are intentionally different shapes (a
   kwarg passthrough vs. a chainable handle override). Both produce the same
   runtime effect — an `Arc<dyn Embedder>` swap on the Rust-side handle — but
   the surfaces diverge to match each ecosystem's idiom. The resulting vectors
   are byte-identical.

---

## Troubleshooting

### `granite-embedding weights missing` (or HF Hub timeout)

First-call symptom: the auto-download is hitting the network.

- Pre-populate the model dir manually (point `model_dir` /
  `LUNARIS_EMBEDDER_DIR` at it), OR
- Set the cache dir to a path your process has write access to, with at least
  1 GB free, AND
- Confirm the runtime has outbound HTTPS to `huggingface.co`.

### Recall quality collapsed after switching to `noop`

`EmbedderConfig.noop()` emits zero vectors — semantic recall is gone by
design. Switch back to `native()` for any non-test path.

### `native_quantized` raises "requires --features embedder-gguf"

The wheel / `.node` was built without the GGUF feature. Either rebuild with
`--features embedder-gguf` (embedder) / `--features reranker-gguf` (reranker),
or use the FP16 `native()` path (always available). The same applies to the
hardware-accelerator features (`metal`, `cuda`, …).

### Ollama escape hatch: connection refused on first ingest

The `embed-remote` path only builds a client at config time; network errors
surface on the first ingest / recall. Confirm `LUNARIS_EMBEDDER_OLLAMA_URL`
points at a reachable Ollama server running the `LUNARIS_OLLAMA_MODEL` model,
and that the wheel was built with `--features embed-remote`.

---

## Next steps

- For the full fastembed/ONNX → native migration (deleted factories, renamed
  env vars, feature map), see
  [`docs/migration/0.3-to-0.4-native-default.md`](../migration/0.3-to-0.4-native-default.md).
- For the cross-SDK parity contract — the test that asserts Python and
  TypeScript SDKs produce byte-identical vectors for the same input — see
  `crates/lunaris-conformance/tests/` (gated behind the SDK-parity cargo
  feature).
- For the underlying Rust crate API, see the rustdoc for
  [`lunaris_embed_native::NativeEmbedder`] and
  [`lunaris_rerank_native::NativeReranker`].

[Ollama]: https://ollama.com/
[`NoopReranker`]: ../../crates/lunaris-rerank/src/noop.rs
[`lunaris_embed_native::NativeEmbedder`]: ../../crates/lunaris-embed-native/src/embedder.rs
[`lunaris_rerank_native::NativeReranker`]: ../../crates/lunaris-rerank-native/src/reranker.rs
