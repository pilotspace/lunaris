# SDK embedder + reranker configuration

Customize the embedder and reranker Lunaris uses for ingest + recall — without
touching environment variables — via the `EmbedderConfig` and `RerankerConfig`
SDK types. This guide covers both the Python (`pip install lunaris`) and
TypeScript (`npm i @pilotspace/lunaris`) SDKs in lockstep; every code sample
appears in both languages.

> **v0.6 llama.cpp-only cutover (ADR 2026-07-10).** Lunaris ships an
> in-process **llama.cpp** embedder + reranker (GGUF artifacts) as the only
> local inference runtime. The v0.4 candle-native paths
> (`EmbedderConfig.native()` / `.native_quantized()`) were deleted — the
> factories still exist but raise immediately with a migration hint. See
> [`docs/migration/0.5-to-0.6-llamacpp-only.md`](../migration/0.5-to-0.6-llamacpp-only.md).

## Overview

`EmbedderConfig` is an opaque handle that wraps a resolved `Arc<dyn Embedder>`
on the Rust side. Construct it via a factory method, then hand it to
`lunaris.open(url, embedder=...)` (Python) or `.withEmbedder(cfg)`
(TypeScript) to override the env-driven default. `RerankerConfig` mirrors the
same shape for the cross-encoder rerank pass.

Use the SDK surface when:

- You want to pin a **specific GGUF artifact** from code rather than relying
  on the `~/.lunaris/models/` staged default.
- You're running Lunaris in a multi-tenant process where each tenant gets a
  different model path.
- You pre-stage GGUFs for an **air-gapped** host and want to point at them
  from code.

Use env vars (`LUNARIS_EMBEDDER_GGUF`, `LUNARIS_RERANKER_GGUF`, …) when you
want one process-wide default with no code changes — both surfaces co-exist,
with the SDK overriding env at handle-construction time.

> **FFI cliff.** You cannot implement the Rust `Embedder` or `Reranker` trait
> from Python or TypeScript. Per-call FFI callbacks would be too slow for the
> hot retrieval path. Roll-your-own backends are a Rust-crate-only escape
> hatch. See [§Limits](#limits) for the full statement.

---

## The customization paths

Each path appears as a `#[staticmethod]` (Python) or `#[napi(factory)]`
(TypeScript) on `EmbedderConfig`. The TS sibling uses camelCase opts bags; the
Python sibling uses keyword arguments. Naming is otherwise identical.

### Path 1 — llama.cpp (default)

Most users land here. `llamacpp()` loads
**`ibm-granite/granite-embedding-311m-multilingual-r2`** (Apache-2.0,
ModernBERT, 768-d) from a **Q4_K_M GGUF** (~240 MiB) via in-process llama.cpp
(static-linked FFI — no external server process). The default artifact
location is `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`.

**Python**

```python
import asyncio
import lunaris
from lunaris import EmbedderConfig

async def main():
    cfg = EmbedderConfig.llamacpp()               # staged default GGUF
    # or pin a pre-staged artifact (air-gapped hosts):
    # cfg = EmbedderConfig.llamacpp(gguf_path="/models/granite-r2.Q4_K_M.gguf")
    mem = await lunaris.open("moon://127.0.0.1:6380", embedder=cfg)
    # ... ingest / recall as usual

asyncio.run(main())
```

**TypeScript**

```typescript
import { open, EmbedderConfig } from "@pilotspace/lunaris";

const cfg = EmbedderConfig.llamacpp();            // staged default GGUF
// or: EmbedderConfig.llamacpp({ ggufPath: "/models/granite-r2.Q4_K_M.gguf" });
const mem = (await open("moon://127.0.0.1:6380")).withEmbedder(cfg);
// ... ingest / recall as usual
```

The TypeScript SDK uses a chainable `withEmbedder` / `withReranker` extension
on the opened handle — each call returns a **new** handle carrying the
override; the original is unaffected. The Python SDK uses
`open(url, embedder=..., reranker=...)` kwargs. Both produce the same runtime
effect — a Rust handle whose `Arc<dyn Embedder>` carries the SDK-supplied
backend.

Explicit construction is **fail-fast**: `llamacpp()` loads the GGUF eagerly
and raises on a missing or corrupt artifact. (The umbrella's env-driven
default resolution instead falls back to `NoopEmbedder` with a `WARN`.)

### Path 2 — Noop

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

### Path 3 — Air-gapped Ollama escape hatch (no SDK factory)

There is **no `EmbedderConfig.ollama()` SDK factory**. The Ollama HTTP
embedder is an **operator-only escape hatch**, selected at build + env time,
not from SDK code:

- Build with `--features embed-remote` on the `lunaris-memory` umbrella.
- Set `LUNARIS_EMBEDDER_OLLAMA_URL` (e.g. `http://localhost:11434`); the
  resolver constructs `lunaris_embed_remote::OllamaEmbedder` when no local
  GGUF is reachable (the llama.cpp step resolves first).
- Optionally set `LUNARIS_OLLAMA_MODEL` (default `embeddinggemma:300m`).

This is **not the supported path** — it exists for air-gapped hosts that
already run an Ollama server. Prefer Path 1 (llama.cpp).

### Retired paths — `native()` / `native_quantized()`

The candle factories were deleted in the llama.cpp-only cutover. They still
exist as stubs so existing code fails with an actionable message instead of an
`AttributeError` / missing-method error:

```python
EmbedderConfig.native()            # raises: removed in the llama.cpp-only cutover…
EmbedderConfig.native_quantized()  # raises: use EmbedderConfig.llamacpp(gguf_path)
```

Migrate by swapping the call — the GGUF artifacts and output contracts are
identical (768-d L2-normalized embeddings; sigmoid rerank scores).

### Pairing with a `RerankerConfig`

`RerankerConfig` mirrors the embedder surface: `llamacpp()` loads
**`BAAI/bge-reranker-v2-m3`** (Apache-2.0, XLM-RoBERTa cross-encoder,
sigmoid output ∈ [0,1]) from a Q5_K_M GGUF (~446 MiB); `noop()` skips the
cross-encoder rescoring pass entirely.

**Python**

```python
from lunaris import EmbedderConfig, RerankerConfig

mem = await lunaris.open(
    url,
    embedder=EmbedderConfig.llamacpp(),
    reranker=RerankerConfig.llamacpp(),
)

# Cheap fallback — disables the cross-encoder rescoring pass:
mem = await lunaris.open(url, embedder=EmbedderConfig.llamacpp(), reranker=RerankerConfig.noop())
```

**TypeScript**

```typescript
import { open, EmbedderConfig, RerankerConfig } from "@pilotspace/lunaris";

const mem = (await open(url))
  .withEmbedder(EmbedderConfig.llamacpp())
  .withReranker(RerankerConfig.llamacpp());

// Cheap fallback — disables the cross-encoder rescoring pass:
const fallback = (await open(url))
  .withEmbedder(EmbedderConfig.llamacpp())
  .withReranker(RerankerConfig.noop());
```

---

## Configuration reference

### `EmbedderConfig.llamacpp`

| Argument | Python type | TS field | Default | Notes |
|---|---|---|---|---|
| `gguf_path` / `ggufPath` | `Path \| str \| None` | `string?` | `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf` | Path to the granite-r2 Q4_K_M GGUF. Loads eagerly; raises if missing/corrupt. |

### `EmbedderConfig.noop`

| Argument | Python type | TS field | Default |
|---|---|---|---|
| `dim` | `int` | `number?` | `768` |

Returns a deterministic zero-vector embedder. The `dim` MUST match the width
your storage index was created with.

### `RerankerConfig.llamacpp`

| Argument | Python type | TS field | Default | Notes |
|---|---|---|---|---|
| `gguf_path` / `ggufPath` | `Path \| str \| None` | `string?` | `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf` | Path to the bge-reranker Q5_K_M GGUF. Loads eagerly; raises if missing/corrupt. |

### `RerankerConfig.noop`

No arguments. Returns a [`NoopReranker`] passthrough; `Hit.rerank_applied`
will be `False` for results that flow through this reranker so callers can
detect the degraded path.

---

## Operational notes

- **GGUF staging.** The default artifact directory is `~/.lunaris/models/`.
  The MCP server stages GGUFs lazily on first recall; other deployments
  download them out-of-band and verify against the canonical SHA-256s printed
  by `cargo run -p lunaris-bench --bin stage-models -- --help`. Multiple
  Lunaris processes can safely share the directory on the same host.
- **Env vars that influence the default.** `LUNARIS_EMBEDDER_GGUF`,
  `LUNARIS_RERANKER_GGUF` (artifact path overrides), and
  `LUNARIS_EMBEDDER_OLLAMA_URL` (requires `--features embed-remote`).
  SDK-supplied values always override env; env overrides nothing (it's the
  bottom of the resolution chain).
- **GPU offload.** CPU is the default device. GPU offload is a per-target
  build-time choice: build the wheel / `.node` (or your Rust binary) with
  `--features metal` (Apple Silicon), `cuda`, or `vulkan` — these forward to
  llama.cpp's backends. `LUNARIS_DEVICE=cpu` is the runtime kill-switch that
  forces zero GPU layers even on an accelerated build.
- **Tier-0 builds.** Artifacts built without the `llamacpp` feature contain
  no inference runtime (and need no C++ toolchain to build). There,
  `llamacpp()` raises a clear "Tier-0 no-inference build" error — use
  `noop()` or install a full build.
- **Cross-SDK consistency.** Vectors produced by `EmbedderConfig.llamacpp()`
  are byte-identical across the Python and TypeScript SDKs for the same input
  (enforced mechanically — see [§Next steps](#next-steps)).

---

## Limits

1. **FFI cliff — no custom trait impls from Python or TypeScript.** The Rust
   `Embedder` and `Reranker` traits require an async fn that returns
   `Vec<Vec<f32>>`. Implementing them from Python / TS would require per-call
   FFI callbacks, which are too slow for the retrieval hot path. The factory
   methods on `EmbedderConfig` / `RerankerConfig` cover every customization the
   Rust crate supports MINUS the "roll your own trait impl" escape hatch, which
   is Rust-only. If you need a backend that doesn't fit the llamacpp / noop
   shape, contribute a constructor to `lunaris-llamacpp` and we'll surface it
   through both SDKs.

2. **`noop` dim MUST match your index.** Moon's `FT.SEARCH` index is pre-sized
   for a fixed embedding width at index-create time. `EmbedderConfig.noop(dim=...)` lets you pick any width,
   so a mismatch against an existing index will corrupt recall — pass the same
   `dim` (768 for granite-r2) the index was built with.

3. **`llamacpp()` requires the `llamacpp` feature.** Tier-0 wheels / `.node`
   artifacts (built with `default-features = false`) raise at config time
   rather than silently falling back to noop.

4. **Weights load in-process.** `llamacpp()` maps the GGUF into llama.cpp in
   the same process as your application. Source artifacts from a trusted
   location and verify the canonical SHA-256s; treat them as you would any
   binary loaded into your process.

5. **The Python `open(url, embedder=...)` kwarg vs. the TypeScript
   `.withEmbedder(cfg)` chain.** These are intentionally different shapes (a
   kwarg passthrough vs. a chainable handle override). Both produce the same
   runtime effect — an `Arc<dyn Embedder>` swap on the Rust-side handle — but
   the surfaces diverge to match each ecosystem's idiom. The resulting vectors
   are byte-identical.

---

## Troubleshooting

### `llamacpp()` raises "failed to open GGUF" (missing artifact)

The GGUF is not staged at the given / default path.

- Download it out-of-band and verify the SHA-256 printed by
  `cargo run -p lunaris-bench --bin stage-models -- --help`, OR
- Point `gguf_path` / `LUNARIS_EMBEDDER_GGUF` at an existing copy, OR
- If you run through the MCP server, let it stage the artifact lazily on
  first recall.

### Recall quality collapsed after switching to `noop`

`EmbedderConfig.noop()` emits zero vectors — semantic recall is gone by
design. Switch back to `llamacpp()` for any non-test path.

### `native()` / `native_quantized()` raises "removed in the llama.cpp-only cutover"

Working as intended — the candle backends were deleted in v0.6. Swap the call
to `llamacpp(gguf_path=...)`; see
[`docs/migration/0.5-to-0.6-llamacpp-only.md`](../migration/0.5-to-0.6-llamacpp-only.md).

### Ollama escape hatch: connection refused on first ingest

The `embed-remote` path only builds a client at config time; network errors
surface on the first ingest / recall. Confirm `LUNARIS_EMBEDDER_OLLAMA_URL`
points at a reachable Ollama server running the `LUNARIS_OLLAMA_MODEL` model,
and that the wheel was built with `--features embed-remote`.

---

## Next steps

- For the candle → llama.cpp migration (deleted factories, renamed features,
  new artifact layout), see
  [`docs/migration/0.5-to-0.6-llamacpp-only.md`](../migration/0.5-to-0.6-llamacpp-only.md).
- For the cross-SDK parity contract — the test that asserts Python and
  TypeScript SDKs produce byte-identical vectors for the same input — see
  `crates/lunaris-conformance/tests/` (gated behind the SDK-parity cargo
  feature).
- For the underlying Rust crate API, see the rustdoc for
  [`lunaris_llamacpp::LlamaCppEmbedder`] and
  [`lunaris_llamacpp::LlamaCppReranker`].

[Ollama]: https://ollama.com/
[`NoopReranker`]: ../../crates/lunaris-rerank/src/noop.rs
[`lunaris_llamacpp::LlamaCppEmbedder`]: ../../crates/lunaris-llamacpp/src/embedder.rs
[`lunaris_llamacpp::LlamaCppReranker`]: ../../crates/lunaris-llamacpp/src/reranker.rs
