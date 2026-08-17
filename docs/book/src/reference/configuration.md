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
| `LUNARIS_GRAPH_ENABLED` | bool | off | Entity / community graph extraction pipeline. (The graph's name is the compile-time constant `LUNARIS_GRAPH_NAME = "lunaris_graph"` — not an env var.) |
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
| `LUNARIS_EMBEDDER_OPENAI_URL` | — | **Selector** for the OpenAI-compatible remote embedder (`lunaris-embed-remote::OpenAiEmbedder`, also `--features embed-remote`): setting it routes embedding to that `/v1/embeddings` endpoint, checked **ahead of** the Ollama hatch (`lunaris/src/handle.rs`). Empty = off. |
| `LUNARIS_EMBEDDER_OPENAI_MODEL` | `text-embedding-3-small` | Model id sent in the OpenAI-compatible `/v1/embeddings` request (`lunaris-embed-remote/src/openai.rs`) |
| `LUNARIS_EMBEDDER_OPENAI_API_KEY` | — | Optional bearer token for that endpoint; empty/whitespace → no `Authorization` header (keyless llama-server/vLLM allowed). Redacted in `Debug` output. |
| `LUNARIS_EMBED_MAX_BATCH_TOKENS` | `4096` | llama.cpp batch-token window for the in-process embedder (`lunaris/src/handle.rs`); values < 16 rejected → default |
| `LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS` | `1024` | Same knob for the interactive/contextd embedder — smaller default (~1.1 GB compute buffer vs ~2.5 GB at 4096); values < 16 rejected → default |

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

**Workspace-wide LLM defaults** (`lunaris-llm`, `src/config.rs`) — consulted
only when the per-pipeline variable (`LUNARIS_EXTRACT_PROVIDER` /
`LUNARIS_VERIFY_PROVIDER` / `LUNARIS_REFLECT_PROVIDER`) is unset. Precedence:
per-pipeline env → workspace env → TOML file → built-in default.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_LLM_PROVIDER` | `ollama` (built-in fallback pair is `ollama` + `gemma3:4b`) | Default provider for the extract/verify/reflect pipelines: `ollama`\|`anthropic`\|`openai`\|`gemini`\|`openai-compat`; unknown value is a hard `UnknownProvider` error |
| `LUNARIS_LLM_MODEL` | `gemma3:4b` | Default model id for those pipelines |
| `LUNARIS_LLM_CONFIG` | unset | Path to a TOML file with optional `[default]`/`[extract]`/`[verify]`/`[reflect]` sections (`provider`, `model`); env vars still win over file values; unreadable path is a hard error |

### Moon storage tuning (`lunaris-storage-moon`)

Read by the Moon adapter in **any** embedding process (server, MCP, SDKs).

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_MOON_OP_TIMEOUT` | `10` (whole **seconds** — no `_MS`/`_SECS` suffix in the name) | Per-command Moon response timeout on the multiplexed connection (`HSET`/`FT.*`/TXN/`PING`), so a stalled Moon cannot hang ingest or recall (`lunaris-storage-moon/src/client.rs`). `≤ 0`/unparseable → warn + default. |
| `LUNARIS_MOON_COMPACT_MIN` | `512` | Minimum vector-upsert count in a bulk ingest before the `BulkIngestComplete` maintenance hint issues `FT.COMPACT` on the scope's vector indexes (`lunaris-storage-moon/src/vector.rs`). `0`/garbage → warn + default. |
| `LUNARIS_MOON_SNAPSHOT_EVERY_COMMIT` | `true` | Whether every `atomic_write` commit also registers a `TEMPORAL.SNAPSHOT_AT` (`lunaris-storage-moon/src/atomic.rs`). Set `false` to save one Moon round trip per write **if you never use `AS_OF` recall**. |

### Supervision / worker pool

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_SCOPE_CONCURRENCY` | `8` | Max concurrent message-process tasks **per scope** |
| `LUNARIS_SCOPE_IDLE_TIMEOUT_MS` | `1800000` (30 min) | Idle-scope worker eviction timeout |
| `LUNARIS_WORKER_DRAIN_MS` | `5000` (5 s) | Graceful drain window when a scope worker shuts down |
| `LUNARIS_CONSOLIDATE_DEBOUNCE_MS` | `60000` (60 s) | Debounce window the consolidation worker waits before flushing a batch of episode events to `consolidate_scoped` (`lunaris-consolidate/src/worker.rs`) |

### Logging

There is **no `LUNARIS_LOG` variable** — the library/server filter is the
standard `RUST_LOG`; the auxiliary binaries each have their own clap-backed
filter var.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_ENV` | — | `production` selects the JSON `tracing` subscriber; otherwise pretty. Also auto-selects JSON when stdout is not a TTY. (`lunaris::init_logging()` / `lunaris::logging::init()`) |
| `RUST_LOG` | `info` when unset | Standard `tracing-subscriber` env filter for the library / `lunaris-server` |
| `LUNARIS_HOOK_LOG` | `warn` | Tracing filter for the `lunaris-hook` binary (`lunaris-hook/src/main.rs`) |
| `LUNARIS_HOOK_LOG_JSON` | unset | `1` switches hook logs to JSON lines |
| `LUNARIS_CONTEXTD_LOG` | `warn` | Tracing filter for the `lunaris-contextd` daemon (`lunaris-hook/src/contextd.rs`) |
| `LUNARIS_MCP_LOG` | `info,rmcp=warn` | Tracing filter for `lunaris-mcp` (stderr — stdout is the MCP transport) |

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
| `LUNARIS_CORS_ORIGINS` | `*` | CORS allow-list — `*` or a comma-separated list. Set an explicit list if browsers talk to your deployment ([Security & Hardening](../operations/security.md)). |
| `LUNARIS_SHUTDOWN_GRACE_SECS` | `30` | Graceful-shutdown drain window |
| `LUNARIS_HTTP_TIMEOUT_SECS` | `30` | Per-request wall-clock budget; exceeding it returns `408`. Covers producing the response, not streaming an SSE body. `0` disables (bound requests at your proxy instead). |
| `LUNARIS_HTTP_CONCURRENCY` | `256` | Max concurrently-served requests; arrivals beyond the cap are **shed** immediately (`503` + `Retry-After`), never queued. `0` disables the limit. |

Plus the `--metrics-disabled` CLI flag, which removes the `/metrics`
endpoint. The per-command Moon timeout the server inherits is
`LUNARIS_MOON_OP_TIMEOUT` — a storage-layer variable, not a
`lunaris-server` flag (see *Moon storage tuning* above).

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

### MCP server (`lunaris-mcp`)

Every var has a matching CLI flag (clap) that takes precedence. Storage
resolution is explicit → contextd-advertised → refuse-to-boot; see
[MCP Server](../mcp/index.md).

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_MCP_STORAGE` | unset (**no default since 0.7.0**) | Storage URL (`moon://host:port`). Unset → adopt a live `lunaris-contextd`-advertised store, else refuse to boot with the external-Moon quickstart. Must match contextd's store or the two write to different Moons. |
| `LUNARIS_MCP_SCOPE` | unset | Overrides the auto-derived memory scope (git remote + branch, else cwd hash) |
| `LUNARIS_MCP_MODELS_DIR` | `~/.lunaris/models` | Where lazy GGUF staging puts model files (mostly test isolation) |
| `LUNARIS_MCP_SKIP_STAGE` | unset | Presence-only: skip lazy GGUF staging on first recall (CI / operator override) |
| `LUNARIS_MCP_DISABLE_CONTEXTD` | unset | Presence-only: serve every op Direct instead of proxying to the warm `lunaris-contextd` daemon |
| `LUNARIS_MCP_CONTEXTD_CONNECT_MS` | `500` | Cold-start budget for connecting to contextd's socket before falling back to Direct |
| `LUNARIS_MCP_CONTEXTD_BREAKER_N` | `3` | Consecutive contextd failures before the circuit breaker opens (ops go Direct) |

(`LUNARIS_MCP_LOG` is in the Logging table above.)

### Hooks & context injection (`lunaris-hook` / `lunaris-contextd`)

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_STORE_URL` | unset | The hook binary's storage URL (`moon://host:port`). Unset → adopt a live contextd-advertised store, else hard-error with the external-Moon quickstart (`lunaris-hook/src/scope.rs`). This is the real name — `LUNARIS_HOOK_STORAGE` does not exist. |
| `LUNARIS_CONTEXT_RECALL` | `hybrid` | `vector` forces the legacy vector-only recall path for context injection; anything else is hybrid (vector + BM25 RRF, degrading to vector on failure) |
| `LUNARIS_CONTEXT_RECALL_TIMEOUT_MS` | `1500` | Deadline on the hybrid retrieve; timeout degrades to the vector path |
| `LUNARIS_CONTEXT_MAX_HITS` / `_MIN_SCORE` / `_MAX_CHARS` | `5` / `0.55` / `1600` | Prompt-phase injection budget: max memories, min cosine score, char cap |
| `LUNARIS_CONTEXT_POST_TOOL_MAX_HITS` / `_MIN_SCORE` / `_MAX_CHARS` | `3` / `0.60` / `900` | Post-tool-call injection budget |
| `LUNARIS_CONTEXT_DIGEST_MAX_HITS` / `_MAX_CHARS` | `8` / `2000` | SessionStart digest budget |
| `LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS` | off | `1`/`true` re-includes raw tool-call captures in prompt-phase injection (excluded by default as low-signal) |
| `LUNARIS_CONTEXT_EMBED_CACHE_MAX` | `256` | Max entries in contextd's query-embedding cache (cleared wholesale when full, not LRU) |
| `LUNARIS_CONTEXT_PROFILE` | off | Exactly `1` (not `true`) emits latency breadcrumbs for recall / embedding / promotion |
| `LUNARIS_INFER_WATCHDOG_MS` | `120000` | Per-inference-call timeout in contextd; a timed-out call fails only that request (recall fail-opens) |
| `LUNARIS_INFER_WATCHDOG_TRIP` | `2` | Consecutive inference timeouts that count as "wedged, not slow" — trips the wedge policy (contextd exits 70; hooks respawn it) |
| `LUNARIS_DREAM_NUDGE_THRESHOLD` | `5` | Ripe-memory count at which the SessionStart digest nudges "run /dream"; `0` disables the check |
| `LUNARIS_SESSIONS_FILE` | `~/.lunaris/sessions.json` | Where session markers / session pads persist (tests + non-default homes) |
| `LUNARIS_CODEX_CONTEXT_*` / `LUNARIS_CODEX_POST_TOOL_*` | same defaults | **Lowest-priority aliases** of the matching `LUNARIS_CONTEXT_*` knobs — consulted only when the generic var is unset/unparseable, for any client (not Codex-detected) |

> `LUNARIS_DREAM_CRON` and `LUNARIS_DREAM_PIGGYBACK` appear in the `/dream`
> skill docs as v2 stubs but are **not read by any code yet** — setting them
> does nothing today.

### Bench-only variables

The `LUNARIS_EVAL_*` family (plus the bench harness knobs) configures the
LongMemEval / PersonaMem benchmark rigs only — never a production process.
They are documented next to the harnesses:
[`scripts/bench/lme/README.md`](https://github.com/pilotspace/lunaris/blob/main/scripts/bench/lme/README.md)
and
[`scripts/bench/pm/README.md`](https://github.com/pilotspace/lunaris/blob/main/scripts/bench/pm/README.md).

### Integration-test probes (not for production)

| Variable | Example | Used by |
|---|---|---|
| `MOON_URL` | `moon://localhost:6390` | `#[cfg(feature = "moon-it")]` tests |
| `LUNARIS_MOON_URL` | `moon://127.0.0.1:6380` (default in the storage-moon live tests) | Live-Moon integration/conformance tests only; unset → those `#[ignore]`d tests skip. Production code never reads it. |
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
