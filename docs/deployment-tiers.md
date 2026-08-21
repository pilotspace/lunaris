# Deployment tiers — build features × memory budget

The llama.cpp-only cutover (ADR
[`2026-07-10-llamacpp-only-cutover.md`](decisions/2026-07-10-llamacpp-only-cutover.md),
decision 5) makes "small devices still run Lunaris" a first-class goal.
This page defines the supported build tiers, what each one costs in
resident memory, and the knobs that move the numbers.

## The tiers

| Tier | Build | Inference | Toolchain | Steady-state RSS (CPU) |
|---|---|---|---|---|
| **Tier-0 — core** | `default-features = false` | none in-process: `NoopEmbedder`/`NoopReranker`; extractor/verifier remote via `cloud-api` | pure Rust — **no cmake, no C++** | ~50 MiB handle baseline¹ |
| **Tier-1 — embed** | `llamacpp` (the default) + `RerankerConfig.noop()` or no reranker GGUF staged | granite-r2 Q4_K_M embedder in-process; reranker Noop | cmake + C++ + libclang⁴ | ~650 MiB peak² |
| **Tier-2 — full** | `llamacpp` + both GGUFs staged | embedder + bge-reranker-v2-m3 Q5_K_M cross-encoder | cmake + C++ + libclang⁴ | ~2.6 GiB high-water³ |
| **GPU variants** | add `metal` / `cuda` / `vulkan` | same models, layers offloaded | + GPU SDK | weights move to VRAM; fixed compute buffers (e.g. ~142 MiB MTL0 + ~88 MiB CPU on Apple Silicon) |

Measurement provenance (this table is only as good as its footnotes —
re-measure when bumping `llama-cpp-2` or the token budget):

1. Empty-handle baseline from the `lazy_reranker_rss` test doc
   (`crates/lunaris/tests/lazy_reranker_rss.rs`): ~50 MiB, and the test
   gates that `Lunaris::open()` does NOT materialize the reranker GGUF
   (N-04 D1 lazy-load contract, re-proven on the llama.cpp path).
2. `/usr/bin/time -l` max RSS of the release `llamacpp_smoke` test
   binary, `LUNARIS_DEVICE=cpu`, 2026-07-10: 654 MiB. Composition:
   ~253 MB Q4_K_M weights (mmap) + ~352 MiB CPU compute buffer (sized by
   the default 8192-token budget) + runtime.
3. Same method on `llamacpp_rerank_smoke` (includes the
   embedder+reranker coexistence test, so both models + both compute
   buffers are resident): 2.64 GiB high-water. This is the worst case —
   a recall-serving process that has executed at least one reranked
   query with both models on CPU.
4. `llama-cpp-sys-2` runs bindgen at build time — building from source
   needs libclang (`libclang-dev` on Debian/Ubuntu; Xcode CLT covers it
   on macOS). Found the hard way on a bare `rust:1.94` container, which
   ships cmake-less AND libclang-less. Wheels / npm / prebuilt binaries
   do not have this requirement.

## Choosing a tier

- **Edge / small device**: Tier-0. Vector recall needs a remote embedder
  (`embed-remote` + `LUNARIS_EMBEDDER_OLLAMA_URL`, or the
  `LUNARIS_EMBEDDER_OPENAI_URL` OpenAI-compatible path) or BYO vectors
  via `ingest_structured`; keyword/graph recall work with no embedder at
  all. Zero C++ in the build — this is the CI cell
  `cargo check -p lunaris-memory --no-default-features`.
- **Default server**: Tier-1 covers ingest + hybrid recall at full
  quality minus the cross-encoder pass (`Hit.rerank_applied = false`
  marks the path without it). That pass is **not** the ~12 ms the
  blueprint budgeted: it measures **p50 1301.3 ms** at the default
  `top_in=60` (575.6 ms at `top_in=30`) on an M4 Pro with full Metal
  offload — see [`docs/operations/capacity.md` §4](operations/capacity.md).
  Tier-1 is therefore the **fast** tier, and Tier-2 buys quality at a
  seconds-class latency budget.
- **Quality-max recall**: Tier-2. The reranker loads lazily on the first
  `rerank()` — a process that never reranks pays Tier-1 RSS.

## Knobs that move the numbers

- **Token budget** (`LlamaCppEmbedderOpts` / `LlamaCppRerankerOpts`
  context sizing): the CPU compute buffer scales with it. Small-device
  deployments that cap chunk length can cut the ~352 MiB buffer
  substantially; re-measure with footnote-2 methodology.
- **`n_threads`**: llama.cpp defaults to physical cores; cap it on
  shared hosts.
- **`LUNARIS_DEVICE=cpu`**: kill-switch that forces zero GPU layers on
  an accelerated build (bounds RSS composition to the CPU numbers
  above).
- **Reranker opt-out**: don't stage the reranker GGUF (resolver falls
  back to `NoopReranker`), or pass `RerankerConfig.noop()` explicitly.

## ARM status

- **aarch64-macOS (Apple Silicon)**: primary dev platform — the full
  workspace battery and all Phase B/C gates ran here.
- **aarch64-linux**: cross-compiled release binaries are built by
  `mcp-prebuild.yml` / `ts-prebuild.yml` / `python-prebuild.yml`
  (g++-aarch64 cross toolchain since Phase D). Runtime smoke on real
  aarch64-linux hardware is tracked as a Phase E follow-up — llama.cpp's
  NEON/dotprod kernels are upstream-supported, but Lunaris has not yet
  published its own on-device measurement.
