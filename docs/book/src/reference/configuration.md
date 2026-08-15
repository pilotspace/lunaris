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
4. **The storage URL scheme** — `moon://…` is the only one 0.7.0 accepts.

Defaults are tuned for *"a Moon you run, a local embedder, graph and verifier
off"* — the safe production floor. Turn things on deliberately.

---

## 1. Cargo feature flags

### `lunaris` (umbrella crate)

> **v0.6 llama.cpp-only cutover.** The candle inference stack (`native`,
> `embedder-gguf`, `reranker-gguf`, `verify-small`, `verify-large`,
> `cpu-accelerate`, `cpu-mkl`, `cuda-fa2`) is deleted. `llamacpp` is the only
> local embed/rerank runtime and is **on by default**; it needs cmake + a C++
> toolchain. See `docs/decisions/2026-07-10-llamacpp-only-cutover.md` (the
> cutover ADR) and `docs/migration/0.5-to-0.6-llamacpp-only.md` (the
> migration guide).

| Feature | Default | Effect |
|---|:--:|---|
| `llamacpp` | ✅ | In-process llama.cpp embedder (`LlamaCppEmbedder`, granite-r2 Q4_K_M GGUF) + reranker (`LlamaCppReranker`, bge-reranker-v2-m3 Q5_K_M GGUF) |
| `metal` | | GPU offload on Apple Silicon — forwards to `llama-cpp-2` |
| `cuda` | | GPU offload via CUDA — forwards to `llama-cpp-2` |
| `vulkan` | | GPU offload via Vulkan — forwards to `llama-cpp-2` |
| `embed-remote` | | Ollama HTTP embedder **escape hatch** (operator-only); activated by `LUNARIS_EMBEDDER_OLLAMA_URL`; resolves **after** the `llamacpp` step |
| `ollama` | | `OllamaExtractor` (`with_extractor`) / Ollama HTTP **verifier** backend selector (NOT the embedder) |
| `cloud-api` | | Cloud-API extractor / verifier backends (pulls `reqwest`) |

`default = ["llamacpp"]`. CPU is the default device; device selection is a
runtime probe, overridden by `LUNARIS_DEVICE=cpu` (the kill-switch that forces
zero GPU layers even on an accelerated build). Extractor and verifier are
**remote-only** — `LUNARIS_EXTRACT_PROVIDER` / `LUNARIS_VERIFY_PROVIDER`
(`anthropic`\|`openai`\|`gemini`\|`minimax`\|`openai-compat`) or a
caller-supplied `with_extractor` / `with_verifier` impl; unset resolves to
`NoopExtractor`/`NoopVerifier`. For a pure-Rust, no-C++-toolchain build
(Tier-0, small devices): `default-features = false` →
`NoopEmbedder`/`NoopReranker`. (The umbrella also forwards `moon-it` /
`pg-it` integration features to the storage crates.)

### `lunaris-llamacpp`

The only local embed/rerank runtime. `LlamaCppEmbedder` and
`LlamaCppReranker` load their GGUF eagerly and raise/log a `WARN` +
`NoopEmbedder`/`NoopReranker` fallback on a missing or corrupt artifact —
there is **no auto-download**; the MCP server stages GGUFs lazily on first
recall, other deployments download them out-of-band and verify against the
canonical SHA-256s printed by `cargo run -p lunaris-bench --bin stage-models
-- --help`.

| Feature | Default | Effect |
|---|:--:|---|
| `metal` / `cuda` / `vulkan` | | Forwarded from the umbrella — GPU offload via `llama-cpp-2` |

### `lunaris-extract`

| Feature | Default | Effect |
|---|:--:|---|
| `ollama` | | `OllamaExtractor` — HTTP `/api/chat` extractor |
| `cloud-api` | | Cloud-API extractor (pulls `reqwest`) |
| `extractor-it` | | Enables integration tests that hit a live Ollama / cloud API |

With no feature/provider active, `NoopExtractor` is the fallback (`applies()
== false`). Provider selection at runtime is via `LUNARIS_EXTRACT_PROVIDER`.

### `lunaris-verify`

| Feature | Default | Effect |
|---|:--:|---|
| *(none)* | ✅ | No verifier backend compiled — `NoopVerifier` only |
| `ollama` | | Ollama HTTP verifier |
| `cloud-api` | | Cloud-API verifier (pulls `reqwest`) |
| `verifier-it` | | Enables integration tests that hit a live Ollama / cloud API |

The candle-based `verify-small` (Gemma-3-270M) / `verify-large` (Gemma-3-27B)
tiers from RFC 0006 were deleted in the v0.6 llama.cpp-only cutover — the
verifier is remote-only (`LUNARIS_VERIFY_PROVIDER`) or `NoopVerifier`, with no
in-process model tier.

### Integration-test features (never default)

`lunaris-recipes` / `lunaris-storage-moon`: `moon-it`. (`pg-it` was removed
in 0.7.0 with the Postgres backend.)
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
| `LUNARIS_VERIFY_PROVIDER` | `anthropic`\|`openai`\|`gemini`\|`minimax`\|`openai-compat` | unset | Remote verifier provider (v0.6 llama.cpp-only cutover deleted the local `270m`/`27b` model tiers — see `docs/decisions/2026-07-10-llamacpp-only-cutover.md`). Set-but-broken logs a `tracing::warn!` and falls back to `NoopVerifier`; unset is also `NoopVerifier`. |
| `LUNARIS_CONSOLIDATOR_BACKEND` | `actr` \| `noop` | `actr` | ACT-R consolidator vs no-op |
| `LUNARIS_GRAPH_ENABLED` | bool | off | Entity / community graph extraction pipeline |
| `LUNARIS_VERIFY_ENABLED` | bool | off | Slow-path arbitration verifier pipeline |
| `LUNARIS_CONSOLIDATE_ENABLED` | bool | off | Consolidation pipeline |

> The llama.cpp granite-r2 embedder and bge-reranker-v2-m3 reranker are
> selected unconditionally when the `llamacpp` feature is on (default) —
> there is no `LUNARIS_EMBEDDER_BACKEND` or `LUNARIS_RERANKER_BACKEND`
> variable. Use the env vars below to change the GGUF path or (operators
> only) redirect the embedder to a remote Ollama endpoint.

### Embedder and reranker details

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_EMBED_CACHE_CAPACITY` | `2048` | Exact-text embedding LRU entries per `Lunaris` handle. Set `0` to disable. |
| `LUNARIS_EMBEDDER_GGUF` | `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf` | Path to the granite-r2 Q4_K_M GGUF loaded by `LlamaCppEmbedder`. No auto-download — a missing/corrupt file logs a `WARN` and falls back to `NoopEmbedder`. |
| `LUNARIS_RERANKER_GGUF` | `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf` | Path to the bge-reranker-v2-m3 Q5_K_M GGUF loaded by `LlamaCppReranker` (lazy-loaded on first recall). Missing/corrupt file logs a `WARN` and falls back to `NoopReranker`. |
| `LUNARIS_DEVICE` | `auto` | `cpu` is the runtime kill-switch forcing zero GPU layers even on a `metal`/`cuda`/`vulkan` build. |
| `LUNARIS_EMBEDDER_OLLAMA_URL` | — | **Operator escape hatch only.** Routes the embedder to a remote Ollama HTTP endpoint; requires `--features embed-remote`; resolves **after** the llama.cpp step. Not the supported path. |
| `LUNARIS_OLLAMA_MODEL` | `embeddinggemma:300m` | Ollama model tag for the `embed-remote` escape-hatch embedder |

### Verifier / extractor providers (remote-only)

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_EXTRACT_PROVIDER` | unset | `anthropic`\|`openai`\|`gemini`\|`minimax`\|`openai-compat` — remote provider for the extractor backend (per-provider key is `<PROVIDER>_API_KEY`, e.g. `ANTHROPIC_API_KEY`); unset is `NoopExtractor` |
| `LUNARIS_VERIFY_API_KEY` | — | Shared API key for the verifier (falls back to the provider-specific env var, e.g. `OPENAI_API_KEY`) |
| `LUNARIS_OPENAI_COMPAT_BASE_URL` | — | Base URL for the `openai-compat` provider (Ollama / llama-server / vLLM / LM Studio); keyless allowed |
| `OPENAI_COMPAT_EXTRACT_MODEL` | — | Model tag for the `openai-compat` extractor |
| `OPENAI_COMPAT_VERIFY_MODEL` | — | Model tag for the `openai-compat` verifier |
| `OLLAMA_URL` | `http://localhost:11434` | Endpoint honoured by `OllamaExtractor` / the Ollama verifier backend (Cargo feature `ollama`, distinct from `openai-compat`) |
| `OLLAMA_EXTRACT_MODEL` | `gemma3:4b` | Model tag for `OllamaExtractor` |
| `OLLAMA_VERIFY_MODEL` | `gemma3:27b` | Model tag for the Ollama verifier backend |

A provider that is set but fails to construct (bad URL, missing key) logs a
`tracing::warn!` and degrades to `NoopExtractor`/`NoopVerifier` — never a
silent backend swap.

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
| `LUNARIS_STORAGE` | *(required)* | Storage URL — `moon://host:port`. No default; no other scheme is accepted. |
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
| `MOON_URL` | `moon://localhost:6390` | `#[cfg(feature = "moon-it")]` tests |
| `MOON_TEST_BINARY` | `/path/to/moon` | `lunaris-test-harness` — the `moon` binary it spawns per fixture. Without it (and without `vendor/moon/target/{release,debug}/moon`) the harness **panics**; there is no in-memory fallback since 0.7.0. |

> Point these at a **dedicated** Moon. Never at a store you care about — the
> fixtures own their instance and clear it.

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
| `moon://host:port` | Moon (Redis-compatible) | Native `FT.SEARCH` vector + BM25, `GRAPH.QUERY`, message queue, native RRF fusion. The adapter creates its `chunks` FT index at the configured embedder's dimension (default 768; `Lunaris::open` passes `embedder.dim()`, `MoonStorage::connect_with_dim` sets it directly) — Moon itself has no dim cap. Start Moon with **`--shards 1`**: an ingest is one MULTI/EXEC transaction and a sharded Moon rejects it. |
| `postgres://` / `postgresql://` / `memory://` / `sqlite:///path` | — | **Removed in 0.7.0.** `StorageError::UnsupportedScheme`, with the message naming `docs/migration/0.6-to-0.7.md`. |
| anything else | — | `StorageError::UnsupportedScheme` |

There is no schema migration and no role bootstrap to run — per-scope
keyspaces, FT indices, GRAPH keys, and MQ topics are created lazily on the
first `atomic_write` per scope.

See also [Operations → The Storage Backend](../operations/backends.md).
