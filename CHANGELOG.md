# Changelog

All notable changes to Lunaris are documented here.
Entries before 0.6.0-rc.1 are preserved raw in [docs/CHANGELOG-archive.md](docs/CHANGELOG-archive.md).

## Unreleased

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
