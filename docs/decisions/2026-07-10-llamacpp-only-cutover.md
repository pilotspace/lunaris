# ADR: llama.cpp-only inference — delete candle, LLM slots go remote-only

- **Date**: 2026-07-10
- **Status**: Accepted (supersedes the "opt-in spike" scope of
  [`2026-07-10-llamacpp-inference-runtime.md`](2026-07-10-llamacpp-inference-runtime.md);
  full reversal of the v0.4 N-03 candle-only cutover)
- **Owners**: Lunaris core
- **Related**: `docs/migration/0.5-to-0.6-llamacpp-only.md`,
  `docs/design/quantized-inference-extractor-reranker.md` (§4b-RESULTS)

## Context

The companion ADR (same date) admitted llama.cpp as an *opt-in* embedder
runtime behind the `llamacpp` feature, driven by the §4b profiling matrix
(candle ~5× below llama.cpp's Metal ceiling for the embedder, ~3× for the
reranker, quantized CPU matmuls bypassing BLAS) and the candle Metal
activation-buffer leak. Phase B then ran the end-to-end gates on the
llama.cpp GGUF stack:

- **LongMemEval-S J = 96.0%** (≥ the 94.0% candle baseline) — **N=50
  SUBSAMPLE** *(annotated 2026-08-21, W3.1)*
- **recall@10 = 98.0%** — **N=50 SUBSAMPLE** *(annotated 2026-08-21, W3.1)*

> **⚠ Annotation added 2026-08-21 — these two gate figures are an N=50
> subsample of `longmemeval_s`, not a dataset-scale result.** Both were
> measured over the first 50 questions. This repository's own benchmark
> write-up describes that exact prefix as **"a sampling artifact, not the
> real number"** — `longmemeval_s` orders its questions with an easy,
> mostly single-session-factual prefix, and q0–49 is not representative
> of the full 500 (`docs/benchmarks/v0.7-longmemeval-jscore-validation.md`,
> written 2026-07-13, six days *after* this ADR relied on the prefix to
> justify deleting the candle stack).
>
> The candle baseline (94.0% / 96.0%) it is compared against is the same
> subsample, so the *relative* comparison — the actual decision input,
> "llama.cpp is not worse than candle" — is sound and the decision is not
> reopened. What is not sound is quoting 96.0% or 98.0% as Lunaris's
> LongMemEval quality. Do not.
>
> The full-dataset figures that later replaced them have themselves been
> retracted; Lunaris currently publishes no LongMemEval headline. See
> [`docs/benchmarks/README.md`](../benchmarks/README.md).
- regression baseline p95 ≤ 0.08 / inversion ≤ 3% — passed

With the quality gates green and the perf gap structural (kernel quality,
not tuning), keeping two local inference runtimes buys maintenance cost and
feature-matrix combinatorics for nothing.

## Decision

1. **llama.cpp is THE inference runtime.** `lunaris-llamacpp`
   (`LlamaCppEmbedder`, granite-r2 Q4_K_M GGUF; `LlamaCppReranker`,
   bge-reranker-v2-m3 Q5_K_M GGUF) is the only local embed/rerank backend.
   The umbrella crate enables `llamacpp` **by default**.
2. **Delete the candle stack.** `lunaris-embed-native`,
   `lunaris-rerank-native`, the candle extractor/verifier backends
   (`candle_gemma3*`), and every `candle-*` workspace dependency are
   removed. `cargo tree` over the workspace shows zero candle crates.
3. **Extractor / verifier are remote-only.** The LLM slots resolve from
   `LUNARIS_EXTRACT_PROVIDER` / `LUNARIS_VERIFY_PROVIDER`
   (anthropic | openai | gemini | minimax | **openai-compat**). The
   `openai-compat` provider is one generic OpenAI-compatible-URL backend
   covering Ollama, llama-server, vLLM, and LM Studio
   (`LUNARIS_OPENAI_COMPAT_BASE_URL`, keyless allowed). Set-but-broken
   config degrades loudly to Noop (warn) — never a silent backend swap.
4. **GPU is per-target build-time.** CPU is the default;
   `metal` / `cuda` / `vulkan` features forward through the umbrella to
   `llama-cpp-2`. Device selection stays a runtime probe + `LUNARIS_DEVICE`.
5. **Tier-0 lightweight builds.** With `llamacpp` off, the workspace
   compiles with **no C++ toolchain and no inference** (NoopEmbedder /
   NoopReranker, remote-or-Noop LLM slots) — the small-device path. SDK
   factories built without the feature raise a clear Tier-0 error.

## Consequences

- One inference runtime to maintain; the candle Metal leak class is gone
  (llama.cpp allocates a fixed compute buffer per context).
- The build requires cmake + a C++ toolchain **when `llamacpp` is on**
  (default). Tier-0 (`--no-default-features`) preserves the pure-Rust,
  zero-C++ build for constrained targets.
- The v0.4 "no external inference process" property still holds:
  llama-cpp-2 is in-process FFI, statically linked.
- Graph extraction and verification now require either a remote provider
  env or a caller-supplied `with_extractor` / `with_verifier` impl. The
  D-10 default (`LUNARIS_GRAPH_ENABLED` off) is unchanged.
- The per-device candle perf gates (`per_device` bench, perf-gates.yml)
  were deleted with their backends; Phase D re-establishes llama.cpp
  perf gates.

## Phasing

Prove-then-delete, workspace green after every wave: A (spike + fixture
pin) → B (gates on the GGUF stack) → C (flip default, delete candle:
C1 default flip, C2 LLM backends, C3 embedder/reranker crates) →
D (CI C++ toolchain, wheels/npm matrix) → E (Tier-0 hardening, ARM smoke).
