# Changelog

All notable changes to Lunaris are documented here.
Entries before 0.6.0-rc.1 are preserved raw in [docs/CHANGELOG-archive.md](docs/CHANGELOG-archive.md).

## Unreleased

### Changed

- **MCP `memory.forget` previews by default (0.6.2 Task F)** — the tool's
  request DTO gained a `dry_run` field that **defaults to `true`**. Omitting it
  now scans and reports instead of deleting; an actual delete requires an
  explicit `"dry_run": false`. Before this change the DTO carried
  `deny_unknown_fields` and no `dry_run` field at all, so an LLM could issue an
  irreversible scope-wide delete and had no way to preview one. The response is
  now `{ status, dry_run, matched, removed }` (flat struct — the rmcp
  `outputSchema` root-object invariant forbids an enum tag). The HTTP
  `POST /v1/forget` surface keeps `dry_run: false` as its default for API
  compatibility; only the MCP surface inverts it.

### Added

- **`ForgetReceipt.matched`** — the number of primitives the target matched,
  populated on every path including `dry_run`, where `rows_written` and
  `rows_deleted` are both zero by construction. Without it a preview could not
  tell the caller what a commit would remove. Additive and
  `#[serde(default)]`, so receipts minted by older servers (the HTTP
  `confirmation_token` carries a serialized prior receipt) still deserialize.

## [0.6.0-rc.2] — 2026-07-17

Second release candidate — fixes two P0-class SDK defects found while
re-validating rc.1 (silent zero-vector recall in the shipped wheels/binaries,
and a deterministic crash at Python process exit), unifies the Moon processor
inside `lunaris-contextd`, and bumps the vendored Moon to v0.8.0.

### Fixed

- **SDK zero-vector P0 (PR #61, `48ec406`)** — `lunaris-py` / `lunaris-ts`
  never forwarded the `llamacpp` (and `metal`/`cuda`/`vulkan`) features to the
  umbrella `lunaris` crate, so every shipped wheel / `.node` binary silently
  fell back to `NoopEmbedder`: default `open()` returned all-zero vectors and
  hybrid recall ranked by BM25 + tie-break only. Both manifests now forward
  the features, pinned by a manifest guard
  (`crates/lunaris-core/tests/sdk_feature_forwarding.rs`) and proven at
  runtime (real semantic scores; recovery TESTs 1–3 pass on the fixed wheel).
- **Python exit crash (PR #64)** — any Python worker that loaded the
  llama.cpp embedder aborted with SIGABRT at normal process exit
  (`GGML_ASSERT([rsets->data count] == 0)` in ggml-metal's static
  destructor). `lunaris-llamacpp` now parks engine state in a takeable
  teardown registry; the Python package auto-registers
  `shutdown_inference()` with `atexit`, so Metal buffers are freed before
  C++ static destructors run. Post-teardown calls return a typed `Closed`
  error. A subprocess regression test asserts exit code 0 after a real embed.
- **Legacy `codex:*` feedback leak (PR #65)** — `excluded_context_source`
  exact-matched the four `lunaris:*` lifecycle literals only, so episodes
  stored before the 2026-07-14 source-prefix rename leaked
  `codex:turn_feedback` / `codex:memory_injection` records into prompt
  injections. The predicate now matches the lifecycle kind for any origin
  prefix; negatives pin that `tool_call` / `decision` / `edit` sources stay
  injectable.
- **Installer Moon identity probe (PR #63)** — `setup-lunaris-agents.py`
  defaulted to port 6380 (the ai-proxy Redis on some boxes); it now defaults
  to 6381 and verifies the endpoint actually speaks Moon (PING + `FT._LIST`)
  before wiring hooks, rejecting a foreign Redis with an actionable error.
- **`LUNARIS_EMBED_BATCH` no longer latched forever (PR #63, closes #49)** —
  the ingest batch-size env override is re-read on every call instead of
  cached in a once-init static.
- **Codex adapter fail-open (`2e475b7`)** — missing hook binaries no longer
  hard-fail the codex adapter.
- **Inference watchdog (`0deb6c4`)** — wedged Metal embeds are bounded; the
  hook exits 70 to self-heal instead of hanging the session.

### Changed

- **contextd embedded-Moon unification (PR #62)** — the Moon processor now
  runs inside the `lunaris-contextd` process (discovery file + loopback-only
  RESP-PING probe); the hook path no longer needs a separately launched Moon.
- **Vendored Moon v0.7.1 → v0.8.0** (plus the dashtable recovery fix); the
  recovery harness now probes the MQ / temporal / graph planes and the #69
  upgrade-replay mode.

### CI / build

- Workspace rustdoc warnings zeroed out (58 sites) and
  `release-preflight.sh` refreshed for the post-v0.6 publish set; the
  three v0.6-era publishable crates (`lunaris-llamacpp`,
  `lunaris-embed-remote`, `lunaris-llm`) now ship READMEs.
- npm pre-release publishes route to the `next` dist-tag.
- The eval-gauntlet workflow is `workflow_dispatch`-only while the
  self-hosted runner pool is empty (a guard test pins the trigger set).
- deps: crossbeam-epoch 0.9.20 (RUSTSEC-2026-0204), spin 0.9.9 (yanked
  upstream).

## [0.6.0-rc.1] — 2026-07-15

First release candidate for 0.6.0 — the **llama.cpp-only inference cutover**
plus five closed milestones. Bundled milestones (attribution in
[RELEASES.md](RELEASES.md)): moon-v030-exploit · claude-code-flagship ·
memory-contract-integrity · hook-session-scratchpad · memory-inspector.

### Added

- **Native-graph hybrid recall (`FT.NAVIGATE`)** — the retrieval DSL fuses
  semantic + BM25 + one-hop graph traversal in a single Moon round trip, so
  linked facts surface without a second query. Recall p50 ~3.9ms @10k.
- **RAPTOR tree retrieval** — `.tree(index, k, depth)` operator plus
  community summaries embedded at ingest, for hierarchical recall over long
  corpora (proven wired and traversed end-to-end).
- **SQ8 scalar-quantized vectors (opt-in)** — `?quant=sq8` handles cut vector
  storage ~4x with recall@k held at the 0.90 floor (CI-gated).
- **Claude Code turnkey install** — a two-command setup wires Lunaris memory
  into a Claude Code session (hybrid recall on the prompt hook, capture on
  tool-use / session events). Session start injects a prior-session digest.
- **Session handover as a first-class event** — switching agent sessions
  drains the working-memory pad and promotes durable facts; a per-session
  scratchpad keeps ephemeral state out of long-term recall.
- **Memory Inspector** — a read-only local dashboard to pick a scope and
  browse episodes, entities, and relations at rest.
- **Proxiable scratchpad + engine ops** — the four `memory.scratchpad_*`
  tools and the engine ops route through `contextd`'s warm per-scope engine
  over a unix socket (with a circuit-breaker fallback to a direct in-process
  engine), so a socket-mode MCP needs no second model load.
- **Framework store adapters** — LangGraph `BaseStore`, CrewAI
  `BaseRAGStorage`, and Letta archival connectors over the memory protocol.
- **Write-time convergence** — deterministic sync dedup of facts plus
  cross-episode contradiction detection with bi-temporal supersede.

### Changed

- **llama.cpp is the sole local inference backend (BREAKING).** The candle
  embedder/reranker/LLM stack is deleted; the embedder
  (granite-embedding-311m, Q4_K_M GGUF) and reranker (bge-reranker-v2-m3,
  Q5_K_M GGUF) now run in-process via `llama-cpp-2`. Extractor / verifier LLM
  slots are **remote-only** (`LUNARIS_EXTRACT_PROVIDER` /
  `LUNARIS_VERIFY_PROVIDER`). SDK entry points are
  `EmbedderConfig.llamacpp()` / `RerankerConfig.llamacpp()`; the retired
  `native()` factories raise a migration hint. Feature `llamacpp` is the
  umbrella default (needs cmake + a C++ toolchain); `default-features = false`
  builds a Tier-0 no-inference binary. GPU is a build-time choice
  (`metal` / `cuda` / `vulkan`), replacing the previous candle Accelerate /
  Metal auto-defaults. See `docs/migration/0.5-to-0.6-llamacpp-only.md`.
- **Priority-lane embedder** — recall-query embeds jump ahead of in-flight
  ingest batches, so a background ingest never blocks an interactive recall.
- **License: Apache-2.0 only** (was dual `Apache-2.0 OR MIT`).

### Fixed

- **contextd socket transport was dead for all memory ops** — the MCP proxy
  framed requests without the `type` discriminator contextd's `ContextRequest`
  decode requires, so every socket call silently fell back to a direct engine.
  Requests are now framed correctly and covered by a wire round-trip test.
- **contextd scope bleed** — the daemon resolved scope from its own birth-env,
  so every repo's captures collapsed into one scope; scope is now resolved
  per-request.
- **Recall filter push-down + hybrid filter gap** — `Filter` now pushes into
  both hybrid branches on Moon (array-category membership matched correctly).
- **Supersede loser is now closed on real backends** — a contradicted fact's
  bi-temporal interval is closed on Moon/Postgres, not just SQLite.
- **11-pattern secret scrubber** on captured hook content (keys, tokens,
  passwords) before it reaches long-term memory.

### Performance

- Single-round-trip `read_as_of`, concurrent hydration fan-out, batched
  `scan_range`, and query-embedding reuse across the two-leg `ThenRetriever`
  reduce recall latency across the Moon storage path.
