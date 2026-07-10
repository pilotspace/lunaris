# Configuration Reference

Lunaris is configured along four axes:

1. **Cargo feature flags** — chosen at *build* time; decide which embedder /
   reranker / extractor / verifier backends are compiled in.
2. **Environment variables** (`LUNARIS_*`) — read at *runtime*, mostly when
   `Lunaris::open` resolves the default pipeline. CLI flags on the
   `lunaris-server` binary override the matching env var (12-factor; see
   CONTEXT.md D-26).
3. **Builder / pipeline toggles** — programmatic switches on the `Lunaris`
   handle (graph pipeline on/off, verifier on/off, per-scope enablement).
4. **The storage URL scheme** — `postgres://…` vs `moon://…` selects the
   backend.

Defaults are tuned for *"Postgres + a local embedder, graph and verifier
off"* — the safe production floor. Turn things on deliberately.

---

## 1. Cargo feature flags

### `lunaris` (umbrella crate)

> **Note.** The native granite-r2 embedder and bge-reranker-v2-m3 reranker
> are **always compiled** into the umbrella — no feature gate, no env-var
> backend swap. The flags below only gate optional extractor / verifier LLM
> backends and escape-hatch paths.

| Feature | Default | Effect |
|---|:--:|---|
| `embedder-gguf` | | Q4_K_M GGUF quantized native embedder; activated by `LUNARIS_EMBEDDER_GGUF` |
| `reranker-gguf` | | Q5_K_M-imatrix GGUF quantized native reranker; activated by `LUNARIS_RERANKER_GGUF` |
| `embed-remote` | | Ollama HTTP embedder **escape hatch** (operator-only); activated by `LUNARIS_EMBEDDER_OLLAMA_URL` |
| `candle` | | In-process candle **extractor** (Gemma-3 4B) + **verifier** (Gemma-3 27B) LLM backends |
| `ollama` | | Ollama HTTP **extractor** + **verifier** backends (NOT the embedder) |
| `cloud-api` | | Cloud-API extractor / verifier backends (pulls `reqwest`) |
| `verify-small` | | Forward `lunaris-verify/verify-small` — the candle Gemma-3 270M laptop-floor verifier (RFC 0006) |
| `verify-large` | | Forward `lunaris-verify/verify-large` — the candle Gemma-3 27B verifier (alias of `candle` on the verify crate) |

`default = []` — the native embedder + reranker are unconditional; no feature
is required for the core recall path. The extractor and verifier pipelines are
opt-in at build time. (The umbrella also forwards `moon-it` / `pg-it`
integration features to the storage crates.)

### `lunaris-embed-native`

| Feature | Default | Effect |
|---|:--:|---|
| `embedder-gguf` | | Q4_K_M GGUF quantized variant (`NativeQuantizedEmbedder`) |
| `embedder-it` | | Gates real-weights integration tests |

Hardware-acceleration variants: `cpu-accelerate`, `cpu-mkl`, `metal`,
`cuda`, `cuda-fa2`. **macOS builds enable Accelerate BLAS automatically**,
and **Apple Silicon builds additionally compile the Metal kernels** via
target-specific dependency blocks (`cpu-accelerate` / `metal` are redundant
no-ops there). Device pick is a **runtime probe** (CUDA → Metal → CPU) with
clean CPU fallback; `LUNARIS_DEVICE=auto|cpu|cuda|metal` overrides it —
`cpu` skips GPU probes (bounds RSS on long-lived servers). `cpu-mkl`,
`cuda`, `cuda-fa2` remain opt-in (external toolchains).

### `lunaris-rerank-native`

| Feature | Default | Effect |
|---|:--:|---|
| `reranker-gguf` | | Q5_K_M-imatrix GGUF quantized variant (`NativeQuantizedReranker`) |
| `reranker-it` | | Gates real-weights integration tests |

Hardware-acceleration variants: `cpu-accelerate`, `cpu-mkl`, `metal`,
`cuda`, `cuda-fa2`. **macOS builds enable Accelerate BLAS automatically**,
and **Apple Silicon builds additionally compile the Metal kernels** via
target-specific dependency blocks (`cpu-accelerate` / `metal` are redundant
no-ops there). Device pick is a **runtime probe** (CUDA → Metal → CPU) with
clean CPU fallback; `LUNARIS_DEVICE=auto|cpu|cuda|metal` overrides it —
`cpu` skips GPU probes (bounds RSS on long-lived servers). `cpu-mkl`,
`cuda`, `cuda-fa2` remain opt-in (external toolchains).

`NoopReranker` is always available (no feature gate) — selectable at runtime
for the latency-floor path.

### `lunaris-extract`

| Feature | Default | Effect |
|---|:--:|---|
| `candle` | ✅ | In-process candle Gemma-3-4B fact extractor |
| `ollama` | | Ollama HTTP extractor |
| `cloud-api` | | Cloud-API extractor (pulls `reqwest`) |
| `extractor-it` | | Enables integration tests that hit a live Ollama / cloud API |

### `lunaris-verify`

| Feature | Default | Effect |
|---|:--:|---|
| *(none)* | ✅ | No verifier backend compiled — `NoopVerifier` only (RFC 0006) |
| `candle` | | In-process candle Gemma-3-27B verifier |
| `verify-small` | | In-process candle Gemma-3-270M verifier — the laptop-floor build (~600 MB weights, ~1 GB RAM, no Ollama). Pulls in `candle`. |
| `verify-large` | | In-process candle Gemma-3-27B verifier — alias of `candle`, named explicitly for RFC 0006 symmetry |
| `ollama` | | Ollama HTTP verifier |
| `cloud-api` | | Cloud-API verifier (pulls `reqwest`) |
| `verifier-it` | | Enables integration tests that load a real Gemma-3 27B model / hit a live Ollama / cloud API |

### Integration-test features (never default)

`lunaris-recipes` / `lunaris-storage-postgres`: `pg-it`.
`lunaris-recipes` / `lunaris-storage-moon`: `moon-it`.
`lunaris-conformance`: `chaos-it`. `lunaris-bench`: `budget-it`.

> **Excluded from `cargo test --workspace`:** `lunaris-py` and `lunaris-ts`
> are `cdylib`s and fail to link under the workspace test runner. Test them
> with `maturin` + `pytest` / `napi build` + `vitest`, or via
> `scripts/sdk-real-evidence.sh`.

---

## 2. Environment variables

All variables below are read by `Lunaris::open` (or the `lunaris-server`
binary) unless noted. Boolean variables accept `1`, `true`, `on` (case-
insensitive) for "enabled"; anything else (or unset) is "disabled".

### Backend / pipeline selection

| Variable | Values | Default | Controls |
|---|---|---|---|
| `LUNARIS_VERIFIER_BACKEND` | `270m`\|`small` \| `27b`\|`large` \| `noop` | `270m` | Which verifier model (RFC 0006); `270m`/`small` needs the `verify-small` feature, `27b`/`large` needs `verify-large` (or `candle`). The effective verifier is still `NoopVerifier` until the matching feature is built **and** the model weights are staged — a cache miss or missing feature logs a `tracing::warn!` and falls back to `NoopVerifier` (the verifier worker stays disabled). |
| `LUNARIS_CONSOLIDATOR_BACKEND` | `actr` \| `noop` | `actr` | ACT-R consolidator vs no-op |
| `LUNARIS_GRAPH_ENABLED` | bool | off | Entity / community graph extraction pipeline |
| `LUNARIS_VERIFY_ENABLED` | bool | off | Slow-path arbitration verifier pipeline |
| `LUNARIS_CONSOLIDATE_ENABLED` | bool | off | Consolidation pipeline |

> The native granite-r2 embedder and bge-reranker-v2-m3 reranker are selected
> unconditionally — there is no `LUNARIS_EMBEDDER_BACKEND` or
> `LUNARIS_RERANKER_BACKEND` variable. Use the env vars below to change the
> model directory, activate a GGUF quantized variant, or (operators only)
> redirect the embedder to a remote Ollama endpoint.

### Embedder and reranker details

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_EMBED_CACHE_CAPACITY` | `2048` | Exact-text embedding LRU entries per `Lunaris` handle. Set `0` to disable. |
| `LUNARIS_EMBEDDER_DIR` | `~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/` | Directory holding native granite-r2 model artifacts |
| `LUNARIS_RERANKER_DIR` | `~/.cache/lunaris/models/bge-reranker-v2-m3/` | Directory holding native bge-reranker-v2-m3 model artifacts |
| `LUNARIS_EMBEDDER_GGUF` | — | Path to a Q4_K_M GGUF file; activates `NativeQuantizedEmbedder` (requires `--features embedder-gguf`) |
| `LUNARIS_RERANKER_GGUF` | — | Path to a Q5_K_M-imatrix GGUF file; activates `NativeQuantizedReranker` (requires `--features reranker-gguf`) |
| `LUNARIS_EMBEDDER_OLLAMA_URL` | — | **Operator escape hatch only.** Routes the embedder to a remote Ollama HTTP endpoint; requires `--features embed-remote`. Not the supported path. |
| `LUNARIS_OLLAMA_MODEL` | `embeddinggemma:300m` | Ollama model tag for the `embed-remote` escape-hatch embedder |
| `OLLAMA_URL` | `http://localhost:11434` | Endpoint honoured by the Ollama extractor / verifier backends |
| `OLLAMA_EXTRACT_MODEL` | `gemma3:4b` | Model tag for the Ollama extractor backend |
| `OLLAMA_VERIFY_MODEL` | `gemma3:27b` | Model tag for the Ollama verifier backend |

### Verifier / extractor cloud-API

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_VERIFY_PROVIDER` | `anthropic` | `anthropic` \| `openai` \| `gemini` — cloud provider for the verifier backend |
| `LUNARIS_VERIFY_API_KEY` | — | Shared API key for the verifier (falls back to the provider-specific env var, e.g. `OPENAI_API_KEY`) |
| `LUNARIS_EXTRACT_PROVIDER` | `anthropic` | `anthropic` \| `openai` \| `gemini` — cloud provider for the extractor backend (per-provider key is `<PROVIDER>_API_KEY`, e.g. `ANTHROPIC_API_KEY`) |

### Supervision / worker pool

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_SCOPE_CONCURRENCY` | `8` | Max concurrent message-process tasks **per scope** |
| `LUNARIS_SCOPE_IDLE_TIMEOUT_MS` | `1800000` (30 min) | Idle-scope worker eviction timeout |
| `LUNARIS_WORKER_DRAIN_MS` | `5000` (5 s) | Graceful drain window when a scope worker shuts down |

### Logging

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_ENV` | — | `production` selects the JSON `tracing` subscriber; otherwise pretty. Also auto-selects JSON when stdout is not a TTY. (`lunaris::init_logging()` / `lunaris::logging::init()`) |
| `RUST_LOG` | — | Standard `tracing-subscriber` env filter |

### HTTP server (`lunaris-server`)

`crates/lunaris-server/src/config.rs` — every var has a matching CLI flag
that takes precedence.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_BIND` | `0.0.0.0:8080` | Listen address |
| `LUNARIS_STORAGE` | *(required)* | Storage URL — `moon://host:port` or `postgres://user:pass@host/db`; the scheme picks the backend |
| `LUNARIS_TOKENS_FILE` | *(required)* | Path to the bearer-token map JSON (see below) |
| `LUNARIS_RATE_PER_SECOND` | `60` | Per-tenant sustained request rate |
| `LUNARIS_RATE_BURST` | `120` | Per-tenant burst budget |
| `LUNARIS_CORS_ORIGINS` | `*` | CORS allow-list — `*` or a comma-separated list |
| `LUNARIS_SHUTDOWN_GRACE_SECS` | `30` | Graceful-shutdown drain window |

Plus the `--metrics-disabled` CLI flag, which removes the `/metrics`
endpoint.

**Bearer-token map format** (`LUNARIS_TOKENS_FILE`, D-07):

```json
{
  "<opaque-bearer-token>": { "tenant": "acme",   "scopes": ["ingest", "recall", "forget"] },
  "<another-token>":       { "tenant": "globex", "scopes": ["recall"] }
}
```

- `tenant` is the **partition scope** for the token (typed as `Scope`,
  validated against `[A-Za-z0-9_\-.]{1,128}`) — and the **only** source of
  truth for it. Route handlers ignore any `scope` / `tenant` field on the
  request body; every public DTO carries `#[serde(deny_unknown_fields)]`, so
  a body that *contains* such a field is rejected (HTTP 422).
- `scopes` is the **verb-permission set** — which of `ingest` / `recall` /
  `forget` this token may call. A request whose route requires a verb not in
  this list is `403`.
- A missing or invalid token is `401`.

### Integration-test probes (not for production)

| Variable | Example | Used by |
|---|---|---|
| `PG_URL` | `postgres://lunaris@localhost/lunaris` | `#[cfg(feature = "pg-it")]` tests |
| `MOON_URL` | `moon://localhost:6380` | `#[cfg(feature = "moon-it")]` tests |

---

## 3. Builder / pipeline toggles (programmatic)

Each opt-in pipeline exposes a handle on `Lunaris` plus an env-seeded initial
state:

| Pipeline | Handle | Env seed | Runtime control |
|---|---|---|---|
| Graph | `GraphPipelineHandle` (`lunaris::graph_pipeline`) | `LUNARIS_GRAPH_ENABLED` (`GRAPH_ENABLED_ENV_VAR`) | `.enable()` / `.disable()` |
| Verify | `VerifierPipelineHandle` (`lunaris::verify_pipeline`) | `LUNARIS_VERIFY_ENABLED` (`VERIFY_ENABLED_ENV_VAR`) | `.enable()` / `.disable()` |
| Consolidate | `ConsolidatorPipelineHandle` (`lunaris::consolidator_pipeline`) | `LUNARIS_CONSOLIDATE_ENABLED` (`CONSOLIDATE_ENABLED_ENV_VAR`) + `LUNARIS_CONSOLIDATOR_BACKEND` | `.enable()` / `.disable()` / `.enable_for_scope(prefix)` (source-prefix filter) |

The handles are obtained from the `Lunaris` value after `open`; see
[Guides → Consolidation & Verification](../guides/consolidate-verify.md) and
[Guides → The Graph Pipeline](../guides/graph.md).

---

## 4. Storage URL scheme

`Lunaris::open(url)` / `lunaris::open(url)` dispatch on the scheme:

| Scheme | Backend | Notes |
|---|---|---|
| `moon://host:port` | Moon (Redis-compatible) | Native `FT.SEARCH` vector + BM25, `GRAPH.QUERY`, message queue, native RRF fusion. The Moon adapter creates its `chunks` FT index at the configured embedder's dimension (default 768; `Lunaris::open` passes `embedder.dim()`, `MoonStorage::connect_with_dim` sets it directly) — Moon itself has no dim cap. See [Choosing a Backend](../operations/backends.md). |
| `postgres://` / `postgresql://[user[:pass]@]host[:port]/db` | Postgres + `pgvector` + Apache AGE + `pgmq` | Native graph + queue; **client-side** RRF fusion. pgvector handles embeddings up to ~1536-d. |
| anything else | — | `StorageError::UnsupportedScheme` |

**Postgres connection details** (`lunaris-storage-postgres/src/pool.rs`):

- Pool size: `max_connections(8)` (currently fixed in code).
- Per-session bootstrap: `LOAD 'age'` + `SET search_path = ag_catalog,
  "$user", public`.
- Migrations run automatically on connect (sqlx-managed, `./migrations/`).
  Use `connect_no_migrate()` for the non-privileged app role that should not
  run DDL — see [Operations → Backends](../operations/backends.md) and
  `docs/migration/0.1-to-0.2.md` §6.2 for the `NOSUPERUSER NOBYPASSRLS` role
  recipe (required so Postgres RLS actually applies).

See also [Operations → Choosing a Backend](../operations/backends.md) for the
trade-off discussion.
