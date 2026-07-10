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

- **LongMemEval-S J = 96.0%** (≥ the 94.0% candle baseline)
- **recall@10 = 98.0%**
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
