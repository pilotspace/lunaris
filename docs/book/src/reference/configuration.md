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
| `LUNARIS_RAPTOR_ENABLED` | bool | **off** | RAPTOR community-tree write at ingest (`crates/lunaris-ingest/src/pipeline.rs`). When on, every ingest builds a hierarchical community tree over the document's headings, summarises each node, embeds the summaries and writes `2 × N` extra ops into the `communities` index. **Nothing on any default recall path reads that index** — `production_root` is `chunks_leg` + `facts_leg` only, and `communities` is queried solely by the opt-in `.tree(..)` DSL operator. Leave it off unless you drive `.tree(..)` yourself. Independent of `LUNARIS_GRAPH_ENABLED`; see `docs/decisions/2026-08-21-gate-raptor-community-write.md`. |
| `LUNARIS_VERIFY_ENABLED` | bool | off | Slow-path arbitration verifier pipeline |
| `LUNARIS_CONSOLIDATE_ENABLED` | bool | off | Consolidation pipeline |
| `LUNARIS_RECALL_RERANK` | bool | off | Opt-in cross-encoder rerank stage on the production recall root (applies to MCP `memory.recall` and HTTP `/v1/recall` / SDK `Lunaris::recall()`; the hook's context-injection hot path never reranks). Read ONCE at handle construction, like `LUNARIS_GRAPH_ENABLED`. |
| `LUNARIS_RECALL_RERANK_TOP_IN` | positive int | `2*k` | Candidate-pool depth fed to the cross-encoder when `LUNARIS_RECALL_RERANK` is on. Clamped to at least the final top-`k`; `0` / non-numeric falls back to the default. |

#### The production recall pipeline (GA-1)

Every production surface builds ONE canonical recall root,
`lunaris_retrieve::production_root(k, graph_enabled)`:

- graph-OFF: `Vector("chunks",k) ∧ BM25("chunks",k) → fuse_rrf(60) → top(k)`
- graph-ON: the same chunks legs fused with the fact legs
  (`Navigate("entities",k, fallback "facts") ∧ BM25("facts",k)`), then `top(k)`

Per-surface deltas on top of that shared root:

| Surface | Fact legs | Activation boost | Cross-encoder rerank |
|---|---|---|---|
| MCP `memory.recall` (+contextd) | with `LUNARIS_GRAPH_ENABLED` | yes (`LUNARIS_ACTIVATION_BOOST`, default on) | opt-in via `LUNARIS_RECALL_RERANK` |
| HTTP `/v1/recall` + SDK `Lunaris::recall()` | with `LUNARIS_GRAPH_ENABLED` | yes (`LUNARIS_ACTIVATION_BOOST`, default on) | opt-in via `LUNARIS_RECALL_RERANK` |
| Hook context injection | always on (`LUNARIS_CONTEXT_RECALL=vector` opts out) | yes (`LUNARIS_ACTIVATION_BOOST`, default on) | never (latency-critical path) |

Rerank is OFF by default on every surface; with it off the bge-reranker GGUF
is never loaded. When enabled, the stage runs between RRF fusion and the
final `top(k)` over the top `LUNARIS_RECALL_RERANK_TOP_IN` candidates.
**Budget seconds, not milliseconds, for it** — the measured cost is
p50 1301.3 ms at `top_in=60` and 575.6 ms at `top_in=30`
([capacity §4](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md)).
It is a quality stage, not a latency-class stage, and turning it on voids
the 25 ms p50 recall contract.

#### Activation boost — **on by default, and it changes ranking**

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_ACTIVATION_BOOST` | **on** (any value except `0`) | Applies the ACT-R activation-ledger prior to the fused candidate set on **every recall surface** — MCP `memory.recall`, contextd, `lunaris-cli`, HTTP `/v1/recall`, hook context injection, and all three SDKs. Memories that have been referenced before rank higher than their raw hybrid score alone would place them. Set `LUNARIS_ACTIVATION_BOOST=0` to opt out (`crates/lunaris/src/recall.rs`). |
| `LUNARIS_BOOST_CACHE_CAPACITY` | `10000` | Entries in the in-process activation-boost lookup cache (`crates/lunaris-verify/src/reflect_apply.rs`). Non-numeric / `0` → default. |

> **Read this before you A/B anything.** The boost is **on by default on
> every surface** (W1.8 — it was previously applied only on the
> `lunaris-memory-service` path, so the same query could rank differently
> through MCP than through `/v1/recall`; that divergence is gone). Any
> benchmark, eval, or regression comparison MUST still state which value it
> ran with, and both arms of an A/B must run with the same one — the prior is
> a real ranking input, not a tiebreaker. It is also the first thing to check
> when recall ordering looks inexplicable in production.
>
> One consequence worth knowing before you read a flamegraph: with the boost
> on, every recall issues **one activation-ledger point read per distinct
> hit** — bounded by the hit set, never by the scope's size, and pinned by
> `ledger_read_cost_is_one_point_read_per_distinct_hit`. Callers that never
> write reinforcement signals still pay those reads and get an empty ledger
> back; `LUNARIS_ACTIVATION_BOOST=0` removes them entirely.

**How much the boost is worth, and for how long.** The prior is additive on a
hit's fused score, bounded by `BOOST_CAP` (0.30, an asymptote — not a value any
real record reaches), and it decays with the age of the memory's most recent
reference. Measured for a memory referenced ten times (`weighted = 30`):

| age since last reference | 10s | 1 min | 10 min | 1 h | 6 h | 1 day | 7 days |
|---|---|---|---|---|---|---|---|
| boost added to score | 0.204 | 0.183 | 0.155 | 0.133 | 0.111 | 0.096 | 0.076 |

A memory that was **never** referenced gets exactly `0.0` — the ledger only ever
promotes, never demotes, so an unreferenced memory keeps its raw hybrid rank.

> Until the fix for **F43** landed, this curve clamped negative ACT-R
> activation to zero, which made
> the prior read exactly `0.0` for any memory older than `weighted²` seconds — 9
> seconds after a single strong reference, 15 minutes after ten. If you are
> reading recall traces from an older build, the boost column is expected to be
> zero almost everywhere; that was a defect (F43), not a tuning choice.



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
| `LUNARIS_SUPPRESS_DEGRADED_WARNING` | — | Silences the one-time stderr line `Lunaris::open` prints when the embedder resolved to `noop` — every vector is zeros, so semantic recall degrades to keyword-only while every call keeps succeeding. Only `1`/`true`/`yes`/`on` suppress; `0`, `false` and an empty value do not. Nothing is printed when a `tracing` subscriber is installed, because that host already receives the `WARN`. Query the backend directly with `embedder_backend()`. |
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
| `LUNARIS_EXTRACT_MODEL` | — | Per-pipeline model override for the extractor. Beats `LUNARIS_LLM_MODEL`; the general form is `LUNARIS_{EXTRACT,VERIFY,REFLECT}_MODEL` (`lunaris-llm/src/config.rs:170`). |
| `LUNARIS_VERIFY_MODEL` | — | Same, for the verifier. |
| `LUNARIS_REFLECT_MODEL` | — | Same, for the reflect pipeline. |
| `LUNARIS_OPENAI_COMPAT_API_KEY` | — | API key for the `openai-compat` provider. Keyless endpoints (llama-server, vLLM, LM Studio) may leave it unset. |

**Precedence for a pipeline's provider/model**, highest first: the per-pipeline
env var (`LUNARIS_EXTRACT_PROVIDER` / `LUNARIS_EXTRACT_MODEL`) → the workspace
env var (`LUNARIS_LLM_PROVIDER` / `LUNARIS_LLM_MODEL`) → the matching section of
`LUNARIS_LLM_CONFIG`'s TOML → the built-in fallback pair (`ollama` +
`gemma3:4b`). Env always wins over the file.

### Moon storage tuning (`lunaris-storage-moon`)

Read by the Moon adapter in **any** embedding process (server, MCP, SDKs).

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_MOON_OP_TIMEOUT` | `10` (whole **seconds** — no `_MS`/`_SECS` suffix in the name) | Per-command Moon response timeout on the multiplexed connection (`HSET`/`FT.*`/TXN/`PING`), so a stalled Moon cannot hang ingest or recall (`lunaris-storage-moon/src/client.rs`). `≤ 0`/unparseable → warn + default. |
| `LUNARIS_MOON_COMPACT_MIN` | `512` | Minimum vector-upsert count in a bulk ingest before the `BulkIngestComplete` maintenance hint issues `FT.COMPACT` on the scope's vector indexes (`lunaris-storage-moon/src/vector.rs`). `0`/garbage → warn + default. |
| `LUNARIS_MOON_SNAPSHOT_EVERY_COMMIT` | `true` | Whether every `atomic_write` commit also registers a `TEMPORAL.SNAPSHOT_AT` (`lunaris-storage-moon/src/atomic.rs`). Set `false` to save one Moon round trip per write **if you never use `AS_OF` recall**. |
| `LUNARIS_MOON_DISCOVERY_TIMEOUT_MS` | `25` | Liveness-probe budget when a hook / MCP / CLI process resolves a Moon URL from the contextd discovery file `~/.lunaris/contextd-moon.url` (`lunaris-core/src/store_discovery.rs`). Deliberately tiny: this runs on the startup path of a one-shot binary, where a **stale** discovery file must cost milliseconds, not seconds. Raise it only if your Moon is genuinely slow to answer `PING`; `0` is rejected (`connect_timeout` refuses a zero duration). |

### Supervision / worker pool

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_SCOPE_CONCURRENCY` | `8` | Max concurrent message-process tasks **per scope** |
| `LUNARIS_SCOPE_IDLE_TIMEOUT_MS` | `1800000` (30 min) | Idle-scope worker eviction timeout |
| `LUNARIS_WORKER_DRAIN_MS` | `5000` (5 s) | Graceful drain window when a scope worker shuts down |
| `LUNARIS_CONSOLIDATE_DEBOUNCE_MS` | `60000` (60 s) | Debounce window the consolidation worker waits before flushing a batch of episode events to `consolidate_scoped` (`lunaris-consolidate/src/worker.rs`) |

### Ingest + embedding batching

The embedder batches by **token window**, not by row count, and the two families
of knob are independent: `*_BATCH_TOKENS` sizes the llama.cpp compute buffer,
`*_BATCH*` sizes how many items the caller hands over at once.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_EMBED_MAX_BATCH_TOKENS` | `4096` | llama.cpp batch-token window for the in-process embedder (`lunaris/src/handle.rs:2105`). Values `< 16` are rejected → default. This is the main RSS lever: ~2.5 GB compute buffer at 4096. |
| `LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS` | `1024` | The same knob for the interactive / contextd embedder. Smaller on purpose (~1.1 GB vs ~2.5 GB) — an interactive path should not hold a server-sized buffer. Values `< 16` → default. |
| `LUNARIS_EMBED_BATCH` | `32` | Rows per embed call on the **ingest** driver (`lunaris-ingest/src/pipeline.rs`). Re-read per batch, so a long-running daemon picks up changes without a restart (issue #49). Values below the built-in `32` are **clamped up**, not honoured — this knob can only raise the batch. |
| `LUNARIS_EMBED_BATCH_SIZE` | `16` | Rows per embed call in the **hook's** embed-promotion worker (`lunaris-hook/src/embed_promotion.rs`). Minimum `1`. |
| `LUNARIS_EMBED_BATCH_WAIT_MS` | `25` | How long that worker waits to accumulate a batch before flushing. |
| `LUNARIS_EMBED_PROMOTION_ENABLED` | `true` | Whether hook-captured rows get embedded (promoted from keyword-only to vector-searchable) at all. |
| `LUNARIS_EMBED_PROMOTION_WORKER` | `true` | Whether promotion runs on a background worker. `false` keeps promotion enabled but inline. |
| `LUNARIS_EMBED_DIM` | `768` | Vector dimension asserted for the `NoopEmbedder` path (`lunaris_core::NOOP_DEFAULT_DIM`). Changing it on a store that already holds vectors re-embeds nothing. |
| `LUNARIS_PREWARM_CONCURRENCY` | `4` | Max concurrent speculative warm-up recall tasks spawned by `ScopedLunaris::end_turn`. Must be a positive integer; `0` / non-numeric / unset → default. One `INFO` line per process records the resolved value. |
| `LUNARIS_GRAPH_EXTRACT_GRANULARITY` | `chunk` | `session` / `episode` / `doc` make graph extraction one LLM call over the whole episode instead of one per chunk — far fewer calls, coarser entities. `chunk` / `chunks` / unset keeps the per-chunk default (`lunaris/src/ingest.rs`). |

### Scope resolution

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_SCOPES_FILE` | `~/.lunaris/scopes.json` | Path to the JSON map from working directory to `Scope`, used by `lunaris-hook` and anything else resolving a scope from cwd (`lunaris-core/src/scope_resolver.rs`). Point it elsewhere for a per-project or per-machine layout. |
| `LUNARIS_HOOK_SCOPE` | — | Hard override: skips cwd resolution entirely and uses this scope. Highest precedence (`lunaris-hook/src/scope.rs`). |
| `LUNARIS_SCOPE` | — | The `--scope` flag's env fallback for `lunaris-cli`. |

### contextd — the shared local daemon

`lunaris-contextd` keeps one warm embedder and one Moon connection for all the
short-lived hook / MCP / CLI processes on a machine. Everything below tunes how
those processes find it and how long they will wait.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_CONTEXTD_SOCKET` | `~/.lunaris/codex-contextd.sock` | Unix socket path. Read **first**, ahead of any store discovery — a developer running contextd would otherwise silently redirect tests and hooks (`lunaris-hook/src/context.rs::default_socket_path`). |
| `LUNARIS_CONTEXTD_EMBEDDED_MOON` | enabled | Set `0` / `false` to stop contextd launching its own in-process Moon; it then requires an external store. |
| `LUNARIS_CONTEXTD_MOON_DIR` | `~/.lunaris/contextd-moon-data` | Data directory for that embedded Moon. An unusable directory logs a `WARN` and disables the embedded path rather than failing the daemon. |
| `LUNARIS_CLI_CONNECT_MS` | `500` | How long `lunaris-cli` waits to connect to the contextd socket before falling back to a direct store connection (`lunaris-cli/src/route.rs`). |
| `LUNARIS_CLI_LOG` | `warn` | Tracing filter for `lunaris-cli` (stderr). |
| `LUNARIS_CONTEXTD_AUTOSTART` | `1` | Whether the Codex hook adapter starts `lunaris-contextd` on demand. `0` / `false` requires you to run it yourself. |
| `LUNARIS_CONTEXTD_BIN` | resolved from `PATH` | Explicit path to the `lunaris-contextd` binary. |
| `LUNARIS_HOOK_BIN` | resolved from `PATH` | Explicit path to the `lunaris-hook` binary. |

#### Codex hook-adapter budgets

`scripts/lunaris-codex-hook-adapter.py` is a thin shim that talks to contextd
over the socket above. These are its per-phase deadlines, in milliseconds; each
one degrades to "inject nothing" rather than delaying the agent.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_CONTEXT_ENABLED` | on | Master switch for context injection through the adapter. Any disabling value turns injection off while leaving capture alone. |
| `LUNARIS_CONTEXT_TIMEOUT_MS` | `300` | Budget for the prompt-phase recall round trip. |
| `LUNARIS_CONTEXT_POST_TOOL_TIMEOUT_MS` | `LUNARIS_CONTEXT_TIMEOUT_MS` (`300`) | Budget for the post-tool-call recall. |
| `LUNARIS_CONTEXT_DIGEST_TIMEOUT_MS` | `LUNARIS_CONTEXT_TIMEOUT_MS` (`300`) | Budget for the SessionStart digest. |
| `LUNARIS_CONTEXT_CAPTURE_TIMEOUT_MS` | `120` | Budget for a capture write. Tighter than recall on purpose — a capture is fire-and-forget. |
| `LUNARIS_CONTEXT_COLD_TIMEOUT_MS` | `15000` | Budget for the **first** request after contextd starts, which pays the model-load cost. |
| `LUNARIS_CONTEXT_CAPTURE_FAST` | on | Fire-and-forget capture path. Turning it off makes captures synchronous — useful when debugging a capture that seems to vanish. |
| `LUNARIS_CONTEXT_CAPTURE_GATE` | `on` | The signal gate that drops low-value captures. `off` is the kill-switch that captures everything. |
| `LUNARIS_HOOK_OUTPUT_TARGET` | `codex` | Output dialect the hook emits (`codex` / `claude`). |

### Hook context injection

What the Claude Code / Codex hook may inject, and the latency budgets it may
spend doing it. Every timeout here is a **hard budget on a user-visible path** —
exceeding one degrades to injecting nothing, never to blocking the turn.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_CONTEXT_RECALL` | `hybrid` | Recall plan for context injection. `vector` opts out of the keyword / graph legs. |
| `LUNARIS_CONTEXT_MAX_CHARS` | `1600` | Character budget for prompt-phase injected context. |
| `LUNARIS_CONTEXT_MIN_SCORE` | `0.55` | Score floor for a hit to be injected at prompt phase. Raise it if injected memories feel irrelevant; lower it if the hook injects nothing. |
| `LUNARIS_CONTEXT_POST_TOOL_MAX_CHARS` | `900` | Same budget for post-tool-call injection (falls back to `LUNARIS_CONTEXT_MAX_CHARS`). |
| `LUNARIS_CONTEXT_POST_TOOL_MIN_SCORE` | `0.60` | Score floor post-tool (falls back to `LUNARIS_CONTEXT_MIN_SCORE`). Higher than the prompt floor on purpose — a mid-turn interruption must clear a higher bar. |
| `LUNARIS_CONTEXT_DIGEST_MAX_CHARS` | `2000` | Character budget for the SessionStart digest. |
| `LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS` | off | `1` restores raw tool-call captures to prompt-phase injection. Off by default because they crowd out durable decisions and edits. |
| `LUNARIS_HOOK_CONTEXT_BUDGET_MS` | `250` | Wall-clock budget for building handover context. Clamped to `[10, 10000]`. |
| `LUNARIS_HOOK_DROP_AFTER_MS` | `100` | Emergency-drop deadline (HOOK-06): past this the hook warns and exits `0` rather than delaying the agent. Clamped to `[10, 10000]`. |
| `LUNARIS_TRANSCRIPT_TAIL_BYTES` | `4194304` (4 MiB) | How much of a transcript file's tail the hook reads per turn. |
| `LUNARIS_HOOK_INCLUDE` | — | Colon-separated glob allow-list of paths the hook may capture. |
| `LUNARIS_HOOK_EXCLUDE` | — | Colon-separated glob deny-list, applied on top of the built-in defaults rather than replacing them. |

The `LUNARIS_CODEX_*` spellings (`LUNARIS_CODEX_CONTEXT_MAX_CHARS`,
`…_CONTEXT_MIN_SCORE`, `…_POST_TOOL_*`) are **legacy aliases** consulted after
the `LUNARIS_CONTEXT_*` names above. Prefer the unprefixed names; the Codex
ones exist so older Codex hook configs keep working.

### Recall degradation signal

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` | `1000` | `recall_with_degraded_check()` flags every returned hit `degraded = true` when the verifier queue depth exceeds this at recall start (`lunaris/src/recall.rs`). Lower it for earlier warning that verification is falling behind. |

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
