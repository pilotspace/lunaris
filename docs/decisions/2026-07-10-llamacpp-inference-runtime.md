# ADR: llama.cpp as an opt-in inference runtime (partial reversal of v0.4 N-03)

- **Date**: 2026-07-10
- **Status**: Accepted (spike scope — embedder first, opt-in feature, not the default runtime)
- **Owners**: Lunaris core
- **Related**: `docs/design/quantized-inference-extractor-reranker.md` (§4b-RESULTS, §4c),
  `docs/migration/0.3-to-0.4-native-default.md` (N-03), `docs/spike/O-02-mlx`

## Context

The v0.4 N-03 cutover consolidated ALL local inference (embedder, reranker,
extractor, verifier) onto candle as the single runtime and deleted the
fastembed/ONNX/production-Ollama paths. That decision bought one backend to
maintain and a pure-Rust build. It predates two kinds of evidence gathered on
2026-07-10:

1. **The §4b profiling matrix** (uncontended host, release builds, per-stage
   tracing spans). Forward is ≥95% of wall time in every device × quant ×
   batch cell, so runtime kernel quality is the only lever left in-process:
   - candle Metal effective throughput is ~2,700 tok/s (granite-311m
     Q4_K_M) vs **13,650 tok/s** for the same GGUF on stock llama.cpp Metal
     (~5× gap), and ~1,800 vs **5,731 tok/s** for bge-reranker-v2-m3 Q5_K_M
     (~3× gap).
   - candle's quantized CPU matmuls bypass BLAS entirely: Q4/Q5 GGUF under
     candle on CPU is a 4–5.6× *slowdown* vs candle FP32+Accelerate.
     llama.cpp's NEON/dotprod quantized kernels are the reason its CPU story
     doesn't have this inversion.
2. **The candle Metal activation-buffer leak.** candle caches Metal compute
   buffers keyed by tensor shape and never frees them within a process; on
   long-haystack LongMemEval ingest this ballooned two eval processes to
   ~12 GB each (26 GB swap, host thrashing) and forces the benchmark harness
   into process-per-question isolation. llama.cpp allocates a fixed compute
   buffer per context — the leak class does not exist there.

The extractor side already measured its own version of this: llama.cpp runs
gemma-3-4b QAT Q4_0 at 789 tok/s prefill / 58.8 tok/s decode on this host's
Metal, vs ~10 tok/s naive for candle's quantized CPU path.

## Decision

Adopt **llama.cpp as an opt-in, in-process inference runtime** via the
`llama-cpp-2` crate (static-linked FFI — no external server process, so the
N-03 "no external inference process in the supported path" property is
preserved even though the single-runtime property is not):

- New backend crate **`crates/lunaris-llamacpp`**, a workspace member whose
  llama.cpp dependency is gated behind its **`llamacpp` feature**. With the
  feature off (the default everywhere), the crate compiles as an empty shell
  in seconds — `cargo test --workspace`, CI clippy, and the published
  binaries never build C++. This mirrors the `embedded-moon` discipline:
  the feature MUST stay out of every default feature set.
- **Embedder first** (`LlamaCppEmbedder`, granite-embedding-311m Q4_K_M,
  CLS-pool + L2-normalize to stay bit-compatible with
  `lunaris-embed-native`'s output contract). It implements the same
  `lunaris_core::Embedder` trait, so the SDK runtime-toggle story
  (`try_with_embedder`) extends naturally.
- Reranker (`bge-reranker-v2-m3`, cross-encoder scoring) and extractor
  (gemma-3-4b QAT) are follow-ups behind the same feature, in that order —
  each must pass the §5 quality gates of the quantization plan doc
  (rerank order-inversion ≤1%, |Δscore| p95 ≤ 0.02; ER-F1 ≥ 0.80) before
  any default flips.
- The GGUF artifacts are byte-identical across runtimes (they were produced
  with llama.cpp tooling), so quality gates compare kernels, not weights.

## What this does NOT decide

- No default-runtime flip. candle remains the supported default for all
  four model slots; llama.cpp is operator-opt-in until the quality gates
  and a soak period say otherwise.
- No removal of any candle path.
- No external `llama-server` subprocess — explicitly rejected (below).

## Alternatives considered

1. **Stay on candle and fix the kernels upstream.** The gap is not one
   kernel: Metal graph scheduling, quantized CPU gemm, and the shape-keyed
   buffer cache are three independent deficits. Upstreaming all three is a
   multi-quarter bet against a moving llama.cpp baseline; we keep candle
   for its Rust ergonomics and revisit as it improves.
2. **`llama-server` subprocess** (OpenAI-compatible HTTP). Near-zero Rust
   code because the HTTP backends exist, but it reverses the N-03
   no-external-process cutover for the *supported* path and adds a
   lifecycle/port/health surface to every deployment. Rejected for the
   supported path; operators can already do this via the `embed-remote`
   escape hatch.
3. **MLX** (`docs/spike/O-02-mlx`). Apple-only; no CUDA story; the Rust
   binding surface is younger than llama-cpp-2's. Parked.
4. **Fix batching only, keep candle as-is.** Length-bucketed batching (§4b
   finding #1) is landing regardless and is complementary — it removes the
   padding tax at the batch-assembly layer. It cannot close the ~5×/~3×
   kernel gap and does not address the Metal buffer leak.

## Consequences

- A C++ toolchain (cmake + clang) becomes a build requirement **only when
  the `llamacpp` feature is enabled**. CI gets one gated job (manual /
  nightly, not per-push) exercising the feature on the self-hosted runner.
- The benchmark harness can eventually drop process-per-question isolation
  (the Metal-leak workaround) for runs that use the llama.cpp embedder —
  the fixed compute buffer removes the leak class that forced it.
- The `default-run` inference story becomes two-runtime: docs must state
  that `llamacpp` is opt-in and which slots it covers as they land.
- Version pinning: `llama-cpp-2` tracks llama.cpp upstream aggressively;
  the workspace pins an exact version and bumps deliberately (same policy
  as the vendored Moon SHA).

## Spike acceptance (this ADR's landing bar)

- `cargo check -p lunaris-llamacpp` (feature OFF) adds ≈0 to workspace
  builds and pulls no C++.
- `cargo test -p lunaris-llamacpp --features llamacpp` compiles llama.cpp,
  loads the staged granite Q4_K_M GGUF, embeds a small batch, and asserts
  dim=768, L2-normalized, non-degenerate (cross-prompt cosine < 0.97), and
  reports tok/s alongside the candle §4b numbers for the same host.

## Spike results (2026-07-10, this host — both acceptance runs green)

- **CPU: ~2,861 tok/s** on a small mixed batch (~310 tokens) INCLUDING
  per-call context creation — **13× candle's CPU-GGUF (217 tok/s)** and at
  parity with candle's *Metal* number (~2,700). The runtime-swap thesis
  holds on the first measurement.
- **Metal: ~1,266 tok/s** on the same tiny batch — slower than its own CPU
  because the spike creates a llama.cpp context per `embed_blocking` call
  and Metal pays that setup hardest; llama-bench's 13,650 tok/s ceiling is
  a warm context at pp512. **Follow-up:** context reuse/pool (keeps the
  fixed-buffer property; removes the per-call setup) before quoting a
  Metal number.
- Integration findings worth keeping: encoder-only models need
  `llama_encode` (llama_decode returns a bare -1); pooled embeddings
  require every token flagged for output; the CONTEXT's `n_seq_max`
  (default 1) — not the batch's — gates multi-sequence packing; packed
  ragged ubatches make identical inputs position-sensitive at the ~1e-3
  element level (cosine ≥ 0.999 — gate on the §5 metrics, not bitwise
  equality, when comparing runtimes).
