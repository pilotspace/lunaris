# SDK embedder + reranker configuration

Customize the embedder and reranker Lunaris uses for ingest + recall — without
touching environment variables — via the `EmbedderConfig` and `RerankerConfig`
SDK types. This guide covers both the Python (`pip install lunaris`) and
TypeScript (`npm i @pilotspace/lunaris`) SDKs in lockstep; every code sample appears in
both languages.

## Overview

`EmbedderConfig` is an opaque handle that wraps a resolved `Arc<dyn Embedder>`
on the Rust side. Construct it via one of four factory methods, then hand it
to `Lunaris.open` (Python) or `Lunaris.withEmbedder` (TypeScript) to override
the env-driven default. `RerankerConfig` mirrors the same shape for the
cross-encoder rerank pass.

Use the SDK surface when:

- Your deployment ships its own ONNX weights (bytes-in-memory or on-disk).
- You want to pin an accelerator (`coreml` on Apple silicon, `cuda` on GPUs)
  from code rather than rebuilding the wheel per environment.
- You're running Lunaris in a multi-tenant process where each tenant gets a
  different embedder.

Use env vars (`LUNARIS_FASTEMBED_CACHE_DIR`, `LUNARIS_OLLAMA_URL`, etc.) when
you want one process-wide default with no code changes — both surfaces
co-exist, with the SDK overriding env at handle-construction time.

> **FFI cliff.** You cannot implement the Rust `Embedder` or `Reranker` trait
> from Python or TypeScript. Per-call FFI callbacks would be too slow for the
> hot retrieval path. Roll-your-own backends are a Rust-crate-only escape
> hatch. See [§Limits](#limits) for the full statement.

---

## The four customization paths

Each path appears as a `#[staticmethod]` (Python) or `#[napi(factory)]`
(TypeScript) on `EmbedderConfig`. The TS sibling uses camelCase opts bags;
the Python sibling uses keyword arguments. Naming is otherwise identical.

### Path 1 — Preset: fastembed

Most users land here. The fastembed preset bundles **EmbeddingGemma 300M**
(768-d output) served via [`fastembed-rs`] + ONNX Runtime. On first call it
downloads the model into `cache_dir` (~600 MB, one-time per host).

**Python**

```python
import asyncio
import lunaris
from lunaris import EmbedderConfig

async def main():
    cfg = EmbedderConfig.fastembed(
        cache_dir="/var/cache/lunaris/fastembed",
        execution="coreml",            # or "cpu" / "cuda"
        show_download_progress=False,
    )
    handle = await lunaris.open("moon://127.0.0.1:6380", embedder=cfg)
    # ... ingest / recall as usual

asyncio.run(main())
```

**TypeScript**

```typescript
import { Lunaris, EmbedderConfig } from "@pilotspace/lunaris";

const cfg = EmbedderConfig.fastembed({
  cacheDir: "/var/cache/lunaris/fastembed",
  execution: "coreml",            // or "cpu" / "cuda"
  showDownloadProgress: false,
});
const handle = (await Lunaris.open("moon://127.0.0.1:6380")).withEmbedder(cfg);
// ... ingest / recall as usual
```

The TypeScript SDK uses a chainable `withEmbedder` / `withReranker` extension
on the `Lunaris` class (Approach B — napi `#[napi] impl` extension); the
Python SDK uses `open(url, embedder=...)` kwargs (Approach A — Python
wrapper). Both produce the same runtime effect — a Rust handle whose
`Arc<dyn Embedder>` carries the SDK-supplied backend.

### Path 2 — Preset: Ollama

Latency escape hatch when you have a local [Ollama] server already running
the same `embeddinggemma:300m` model. No in-process ONNX load — the SDK
issues HTTP calls to `/api/embed`.

**Python**

```python
cfg = EmbedderConfig.ollama(
    endpoint="http://localhost:11434",   # default
    model="embeddinggemma:300m",         # default
    dim=768,                             # default; must match server output
)
handle = await lunaris.open(url, embedder=cfg)
```

**TypeScript**

```typescript
const cfg = EmbedderConfig.ollama({
  endpoint: "http://localhost:11434",   // default
  model: "embeddinggemma:300m",         // default
  dim: 768,                             // default; must match server output
});
const handle = (await Lunaris.open(url)).withEmbedder(cfg);
```

The constructor only builds a `reqwest::Client`; network errors surface on
the first ingest / recall, not at config time.

### Path 3 — BYO ONNX bytes

Bring your own ONNX model when the bytes are already in memory — e.g. fetched
from S3 or a model registry, decrypted from a secret store, etc. The SDK
takes ownership of the bytes and constructs a fastembed user-defined session.

**Python**

```python
import asyncio
import lunaris
from lunaris import EmbedderConfig

async def main():
    onnx_bytes = open("/models/my-bert.onnx", "rb").read()
    tok_bytes = open("/models/my-bert.tokenizer.json", "rb").read()

    cfg = EmbedderConfig.from_onnx_bytes(
        onnx_bytes=onnx_bytes,
        tokenizer_bytes=tok_bytes,
        dim=768,                  # MUST match the ONNX graph output
        pooling="mean",           # or "cls"
        execution="cpu",
        # Optional HF auxiliaries:
        tokenizer_config_bytes=None,
        special_tokens_map_bytes=None,
        config_bytes=None,
    )
    handle = await lunaris.open("moon://127.0.0.1:6380", embedder=cfg)

asyncio.run(main())
```

**TypeScript**

```typescript
import { readFile } from "node:fs/promises";
import { Lunaris, EmbedderConfig } from "@pilotspace/lunaris";

const onnxBytes = await readFile("/models/my-bert.onnx");
const tokBytes = await readFile("/models/my-bert.tokenizer.json");

const cfg = EmbedderConfig.fromOnnxBytes({
  onnxBytes,
  tokenizerBytes: tokBytes,
  dim: 768,                       // MUST match the ONNX graph output
  pooling: "mean",                // or "cls"
  execution: "cpu",
  // Optional HF auxiliaries:
  tokenizerConfigBytes: undefined,
  specialTokensMapBytes: undefined,
  configBytes: undefined,
});
const handle = (await Lunaris.open("moon://127.0.0.1:6380")).withEmbedder(cfg);
```

The BYO factories wrap the resolved embedder in `DimValidatingEmbedder`,
which asserts that the first emitted vector matches `dim` — a mismatch
raises a `LunarisError` instead of silently corrupting your vector index.
See [§Limits](#limits) for the contract.

### Path 4 — BYO ONNX path

Same as Path 3, but the SDK reads the bytes off disk for you. Use when the
operator hands you filesystem paths (e.g. mounted from a model volume) and
you don't want to pre-load into Python / Node memory.

**Python**

```python
cfg = EmbedderConfig.from_onnx_path(
    onnx_path="/models/my-bert.onnx",
    tokenizer_path="/models/my-bert.tokenizer.json",
    dim=768,
    pooling="mean",
    execution="cpu",
    # Optional HF auxiliaries:
    tokenizer_config_path=None,
    special_tokens_map_path=None,
    config_path=None,
)
handle = await lunaris.open(url, embedder=cfg)
```

**TypeScript**

```typescript
const cfg = EmbedderConfig.fromOnnxPath({
  onnxPath: "/models/my-bert.onnx",
  tokenizerPath: "/models/my-bert.tokenizer.json",
  dim: 768,
  pooling: "mean",
  execution: "cpu",
  // Optional HF auxiliaries:
  tokenizerConfigPath: undefined,
  specialTokensMapPath: undefined,
  configPath: undefined,
});
const handle = (await Lunaris.open(url)).withEmbedder(cfg);
```

File-read failures raise `LunarisError` with the failing field name + path
in the message (e.g. `"failed to read tokenizer_path=/x.json: No such file
or directory"`).

### Pairing with a `RerankerConfig`

`RerankerConfig` ships two factories today: `fastembed` (BGE-Reranker-v2-m3
cross-encoder) and `noop` (RETRIEVE-06 passthrough). BYO ONNX for the
reranker is **deferred** — see [§Limits](#limits).

**Python**

```python
from lunaris import EmbedderConfig, RerankerConfig

emb = EmbedderConfig.fastembed(cache_dir="/var/cache/lunaris/fastembed")
rer = RerankerConfig.fastembed(cache_dir="/var/cache/lunaris/fastembed-reranker")
handle = await lunaris.open(url, embedder=emb, reranker=rer)

# Cheap fallback — disables the cross-encoder rescoring pass:
handle = await lunaris.open(url, embedder=emb, reranker=RerankerConfig.noop())
```

**TypeScript**

```typescript
import { Lunaris, EmbedderConfig, RerankerConfig } from "@pilotspace/lunaris";

const emb = EmbedderConfig.fastembed({ cacheDir: "/var/cache/lunaris/fastembed" });
const rer = RerankerConfig.fastembed({ cacheDir: "/var/cache/lunaris/fastembed-reranker" });
const handle = (await Lunaris.open(url)).withEmbedder(emb).withReranker(rer);

// Cheap fallback — disables the cross-encoder rescoring pass:
const fallback = (await Lunaris.open(url))
  .withEmbedder(emb)
  .withReranker(RerankerConfig.noop());
```

---

## Configuration reference

### `EmbedderConfig.fastembed`

| Argument | Python type | TS field | Default | Notes |
|---|---|---|---|---|
| `cache_dir` / `cacheDir` | `Path \| str \| None` | `string?` | `$LUNARIS_FASTEMBED_CACHE_DIR` → `~/.cache/lunaris/models/fastembed/` | Directory for the auto-downloaded ONNX weights. |
| `execution` | `str` | `string?` | `"cpu"` | One of `"cpu"`, `"coreml"`, `"cuda"`. Maps to [`lunaris_embed::fastembed::ExecutionPreference`]. Python rejects unknown values strictly; TS warns + falls back to CPU on a non-feature-gated build. |
| `show_download_progress` / `showDownloadProgress` | `bool` | `boolean?` | `False` | Emit fastembed's progress bar to stderr. |

### `EmbedderConfig.from_onnx_bytes` / `EmbedderConfig.fromOnnxBytes`

| Argument | Python type | TS field | Required | Notes |
|---|---|---|---|---|
| `onnx_bytes` / `onnxBytes` | `bytes` | `Buffer` | yes | Raw `model.onnx` bytes. |
| `tokenizer_bytes` / `tokenizerBytes` | `bytes` | `Buffer` | yes | Raw HF `tokenizer.json` bytes. |
| `dim` | `int` | `number` | yes | Declared output dim. Validated against the model's first batch output. |
| `pooling` | `str` | `string?` | no | `"mean"` (default) or `"cls"`. Maps to [`lunaris_embed::fastembed::PoolingMode`]. |
| `tokenizer_config_bytes` / `tokenizerConfigBytes` | `bytes \| None` | `Buffer?` | no | Optional HF `tokenizer_config.json`. |
| `special_tokens_map_bytes` / `specialTokensMapBytes` | `bytes \| None` | `Buffer?` | no | Optional HF `special_tokens_map.json`. |
| `config_bytes` / `configBytes` | `bytes \| None` | `Buffer?` | no | Optional HF `config.json`. |
| `execution` | `str` | `string?` | no | Same enum as the preset path. |

`max_length` is fixed at [`FASTEMBED_GEMMA_MAX_TOKENS`] internally — there is
no SDK knob today.

### `EmbedderConfig.from_onnx_path` / `EmbedderConfig.fromOnnxPath`

Same shape as `from_onnx_bytes`, with every `*_bytes` argument replaced by
the corresponding `*_path` (Python: `Path | str`; TS: `string`). The SDK
performs `std::fs::read` on each path and delegates to the bytes factory.

### `EmbedderConfig.ollama`

| Argument | Python type | TS field | Default |
|---|---|---|---|
| `endpoint` | `str \| None` | `string?` | `$LUNARIS_OLLAMA_URL` → `http://localhost:11434` |
| `model` | `str \| None` | `string?` | `$LUNARIS_OLLAMA_MODEL` → `embeddinggemma:300m` |
| `dim` | `int` | `number?` | `768` |

### `RerankerConfig.fastembed`

| Argument | Python type | TS field | Default |
|---|---|---|---|
| `cache_dir` / `cacheDir` | `Path \| str \| None` | `string?` | `$LUNARIS_FASTEMBED_RERANKER_CACHE_DIR` → `~/.cache/lunaris/models/fastembed-reranker/` |
| `execution` | `str` | `string?` | `"cpu"` |
| `show_download_progress` / `showDownloadProgress` | `bool` | `boolean?` | `False` |

### `RerankerConfig.noop`

No arguments. Returns a [`NoopReranker`] passthrough; `Hit.rerank_applied`
will be `False` for results that flow through this reranker so callers can
detect the degraded path.

---

## Operational notes

- **First-call HF Hub fetch.** The fastembed presets pull weights from
  Hugging Face Hub on first use. Set `cache_dir` (or
  `LUNARIS_FASTEMBED_CACHE_DIR`) to a path with write permission and ~1 GB
  free. The download is one-time per host; subsequent processes reuse the
  cache.
- **Cache layout.** Inside `cache_dir`, fastembed creates per-model
  subdirectories (`models--<org>--<name>/`) following the HF Hub layout.
  Multiple Lunaris processes can safely share the same `cache_dir` on the
  same host.
- **Env vars that influence the default.** `LUNARIS_FASTEMBED_CACHE_DIR`,
  `LUNARIS_FASTEMBED_RERANKER_CACHE_DIR`, `LUNARIS_OLLAMA_URL`,
  `LUNARIS_OLLAMA_MODEL`. SDK-supplied values always override env; env
  overrides nothing (it's the bottom of the resolution chain).
- **Accelerator features.** Building the SDK wheel / `.node` artifact with
  `--features lunaris-embed/fastembed-coreml` or `fastembed-cuda` is
  required for the matching `execution=` values. Without the feature:
  Python raises `ValueError` at config time; TypeScript warns and silently
  falls back to CPU. See `.planning/phases/20-fastembed-adoption/20-01-SUMMARY.md`
  for the upstream rollout details.
- **Cross-SDK consistency.** Vectors produced by `EmbedderConfig.fastembed()`
  are byte-identical across the Python and TypeScript SDKs for the same
  input (enforced mechanically — see [§Next steps](#next-steps)).

---

## Limits

1. **FFI cliff — no custom trait impls from Python or TypeScript.** The Rust
   `Embedder` and `Reranker` traits require an async fn that returns
   `Vec<Vec<f32>>`. Implementing them from Python / TS would require per-call
   FFI callbacks, which are too slow for the retrieval hot path. The four
   factory methods on `EmbedderConfig` (and two on `RerankerConfig`) cover
   every customization the Rust crate supports MINUS the "roll your own
   trait impl" escape hatch, which is Rust-only. If you need a backend that
   doesn't fit the preset / Ollama / BYO-ONNX shape, contribute a new
   constructor to `lunaris-embed` and we'll surface it through both SDKs.

2. **BYO models MUST match the declared `dim`.** Both `from_onnx_bytes` and
   `from_onnx_path` wrap the resolved embedder in `DimValidatingEmbedder`.
   The wrapper inspects the first batch's output length and, if it
   disagrees with the `dim` argument, raises a `LunarisError` with the
   message `"declared dim X does not match observed dim Y — check the ONNX
   model output shape"`. The check is one-shot (the ONNX graph shape
   doesn't drift call-to-call); subsequent batches incur zero overhead.
   Lunaris's storage backends (Moon FT.SEARCH, Postgres pgvector) are
   pre-sized for a fixed `dim`, so a silent mismatch would corrupt your
   index; this wrapper turns the failure mode into a clear startup error.

3. **Reranker BYO is deferred.** fastembed 5.13.4 upstream exposes
   `TextRerank::try_new_from_user_defined`, but the Lunaris
   `lunaris-rerank::FastembedReranker` wrapper does not yet plumb it
   through (Phase 19-04 / 20-01 left it as a follow-up). The SDK can only
   wrap what the Rust crate exposes today, so `RerankerConfig` ships
   `fastembed()` + `noop()` only. A follow-up phase will add
   `RerankerConfig.from_onnx_bytes` / `from_onnx_path` once the Rust-side
   wrapper lands. The cross-SDK parity test catches divergence if one
   language ships BYO first.

4. **Model bytes execute in-process.** The ONNX bytes you pass to
   `from_onnx_bytes` / `from_onnx_path` run inside ONNX Runtime in the same
   process as your application. Lunaris does NO graph validation. Source
   the bytes from a trusted registry; treat them as you would any other
   binary that gets loaded into your process.

5. **The Python `Lunaris.open(url, embedder=...)` kwarg vs. the TypeScript
   `.withEmbedder(cfg)` chain.** These are intentionally different shapes
   (Approach A vs. Approach B in Plans 21-01 and 21-02 respectively). Both
   produce the same runtime effect — an `Arc<dyn Embedder>` swap on the
   Rust-side handle — but the surfaces diverge to match each ecosystem's
   idiom. A cross-language port is one mechanical translation; the
   resulting vectors are byte-identical.

---

## Troubleshooting

### `fastembed: failed to fetch model ...` (or HF Hub timeout)

First-call symptom: the auto-download is hitting the network.

- Pre-populate `cache_dir` with the model files manually, OR
- Set `LUNARIS_FASTEMBED_CACHE_DIR` to a path your process has write access
  to, with at least 1 GB of free space, AND
- Confirm the runtime has outbound HTTPS to `huggingface.co`.

### `LunarisError: declared dim 768 does not match observed dim 512 — check the ONNX model output shape`

You passed `dim=768` to `from_onnx_bytes` / `from_onnx_path` but the model
emits 512-d vectors.

- Inspect the model's output shape (Netron, `onnx.checker`, or print the
  tensor shape during a one-off ORT session).
- Update the `dim` argument to match. Note that Moon's `FT.SEARCH` is
  pre-sized for the embedder's dim at index-create time — changing `dim`
  on an existing Lunaris deployment requires re-indexing.

### `ValueError: EmbedderConfig: unknown execution "metal" — expected one of "cpu", "coreml", "cuda"`

The strict Python parser caught a typo. Use one of the three accepted
values; on Apple silicon, you want `"coreml"`. The TypeScript SDK's
parser warns and silently uses `cpu` for the same input — Python errors
because operators typically run from a REPL where the strict failure mode
is more useful than a silent warning.

### `EmbedderConfig: execution="coreml" requires the lunaris-py wheel to be built with the lunaris-embed/fastembed-coreml feature`

The wheel was built without the accelerator feature. Either:

- Rebuild from source with `maturin develop --release --features lunaris-embed/fastembed-coreml`, OR
- Use `execution="cpu"` (CPU path always available).

The same applies to `fastembed-cuda` for NVIDIA GPUs.

### `failed to read onnx_path=/models/my-bert.onnx: No such file or directory`

`from_onnx_path` resolves paths from the process's cwd; pass an absolute
path or ensure the relative path resolves correctly. The error message
includes the failing field name (e.g. `tokenizer_config_path=...`) so
you can tell which of the four optional paths blew up.

---

## Next steps

- For more context on how Lunaris adopted fastembed and the BYO ONNX path,
  see `.planning/phases/20-fastembed-adoption/20-01-SUMMARY.md`
  (a public migration guide for v0.1 → v0.2 will land in
  `docs/migration/0.1-to-0.2.md`).
- For the cross-SDK parity contract — the test that asserts Python and
  TypeScript SDKs produce byte-identical vectors for the same input — see
  `crates/lunaris-conformance/tests/sdk_embedder_parity.rs` (gated behind
  the `sdk-parity-it` cargo feature).
- For the underlying Rust crate API, see the rustdoc for
  [`lunaris_embed::fastembed::FastembedEmbedder`] and
  [`lunaris_embed::ollama::OllamaEmbedder`].

[`fastembed-rs`]: https://github.com/Anush008/fastembed-rs
[Ollama]: https://ollama.com/
[`lunaris_embed::fastembed::ExecutionPreference`]: ../../crates/lunaris-embed/src/fastembed_exec.rs
[`lunaris_embed::fastembed::PoolingMode`]: ../../crates/lunaris-embed/src/fastembed.rs
[`lunaris_embed::fastembed::FastembedEmbedder`]: ../../crates/lunaris-embed/src/fastembed.rs
[`lunaris_embed::ollama::OllamaEmbedder`]: ../../crates/lunaris-embed/src/ollama.rs
[`FASTEMBED_GEMMA_MAX_TOKENS`]: ../../crates/lunaris-embed/src/fastembed.rs
[`NoopReranker`]: ../../crates/lunaris-rerank/src/noop.rs
