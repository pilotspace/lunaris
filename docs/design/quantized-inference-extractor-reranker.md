# Quantized inference for the extractor + reranker

**Status:** PROPOSED (2026-07-10) · owner: Tin Dang
**Branch target:** follow-on to `feat/moon-v051-perf-exploit`
**Scope:** inference-cost optimization of the two heaviest local models —
the Gemma-3 4B extractor and the bge-reranker-v2-m3 cross-encoder — via
weight quantization, with data-scientist-grade quality gates so no
accuracy regression ships silently.

---

## 1. Where the inference cost actually is (investigated 2026-07-10)

| Model | Role | Today's production path | Cost profile |
|---|---|---|---|
| `gemma-3-4b-it` (extractor) | Fact/Entity/Relation extraction when the graph pipeline is ON | `lunaris_llm::CandleBackend` loads `model.safetensors` at **`DType::F32`** on **`Device::Cpu`** (`crates/lunaris-llm/src/candle.rs:141`, default device `crates/lunaris-extract/src/candle_gemma3.rs:100`), flash-attn off | ~16 GB weight RSS (4B × 4 bytes), CPU-only greedy decode. The 150 ms default batch timeout is unwinnable at this cost — which is why every live graph-pipeline run (LongMemEval A/B) routes extraction to a **cloud API** instead. The native extractor is effectively dead weight today. |
| `bge-reranker-v2-m3` (reranker) | Cross-encoder rerank on every production recall | FP32 `NativeReranker` **default**; Q5_K_M-imatrix GGUF exists but is **opt-in** (`LUNARIS_RERANKER_GGUF` + `reranker-gguf` feature). Quantized path already routes through `device_select` → Metal on Apple Silicon. | FP32 = 2.3 GB weights + FP32 gemm. The Q5 GGUF (447 MB) is what all recent LME validation runs actually use — the shipped default is the path nobody runs. |
| `gemma-3-270m-it` (verifier-small) | verify pipeline | Same F32/CPU `CandleBackend` | Small enough that quantization is a "later" (adjacent, §7). |

Supporting evidence:
- candle 0.10.2 ships `candle_transformers::models::quantized_gemma3` —
  a GGUF-native Gemma-3 forward pass with quantized matmul kernels
  (CPU SIMD + Metal). We already depend on this exact candle version.
- Google publishes **QAT** (quantization-aware-trained) Q4_0 GGUFs for
  gemma-3-4b-it (`google/gemma-3-4b-it-qat-q4_0-gguf`) — trained to keep
  quality at 4-bit, not post-hoc rounded. Community imatrix Q4_K_M is the
  fallback.
- The repo already has the full GGUF playbook from the embedder/reranker:
  conversion (`scripts/spike-convert-bge-reranker-to-gguf.sh`), imatrix
  calibration (`scripts/spike-imatrix-bge-rerank.sh`), SHA-256 pinning,
  reference-score parity tests (`tests/quantized_equivalence.rs` pattern),
  and the `LUNARIS_*_GGUF` env + feature-flag wiring in
  `crates/lunaris/src/handle.rs` (`resolve_embedder`/`resolve_reranker`).

## 2. Objectives (project-owner view)

1. **Make the native extractor viable** so the graph pipeline can run
   without a cloud API: ≥5× decode throughput vs the F32/CPU baseline and
   weight RSS ≤ 3 GB. This is the gap between "graph RAG is a demo behind
   an API key" and "graph RAG runs on the customer's box".
2. **Close the reranker default/reality gap**: promote the quantized
   reranker from opt-in env hack to a first-class, validated default tier,
   and establish whether Q4_K_M-imatrix can replace Q5_K_M for another
   ~25% size/latency win at score parity.
3. **No silent quality loss** — every quant tier ships with a pinned,
   reproducible parity report (§5). Near-zero-hallucination positioning
   dies if the extractor starts inventing entities at 4-bit.

## 3. Workstream A — extractor Q4 GGUF path (the headline)

New `QuantizedCandleBackend` in `lunaris-llm` mirroring the
embedder/reranker GGUF pattern:

- `candle_transformers::models::quantized_gemma3::ModelWeights` loaded
  from GGUF via `gguf_file::Content` (single file, mmap-friendly, no
  16 GB F32 materialization).
- Device via the same `select_device` ladder the reranker uses
  (Cpu → Metal(0)/Cuda(0) when the feature is on). Quantized matmuls have
  Metal kernels in candle 0.10 — decode on Metal, sampling on CPU.
- Weights: prefer **Google QAT Q4_0** (quality-preserving by training);
  benchmark against community **Q4_K_M-imatrix** and pick by the §5
  gates. SHA-256 pinned like `BGE_RERANKER_GGUF_Q4_SHA256`.
- Tokenizer/config: keep the HF `tokenizer.json` + `config.json` as
  source of truth (the GGUF metadata is NOT trusted — same lesson as the
  reranker's `pooling_type` incident).
- Wiring: `LUNARIS_EXTRACTOR_GGUF=<path>` + `extractor-gguf` feature in
  `lunaris-extract` → umbrella passthrough, falling back to the F32 path
  then `NoopExtractor` exactly as today (fail-open contract preserved,
  D-21 semantics untouched).
- Timeouts: the 150 ms/450 ms defaults were tuned for a fantasy; re-derive
  from measured Q4 decode tok/s (§5 bench) so the default batch path
  actually completes locally.

Expected effect (to be proven, not assumed): weights 15.9 GB (F32) →
~2.4 GB Q4 (~6.6× smaller); CPU decode gains from 4-bit SIMD gemm;
Metal decode unlocked (F32-on-CPU was the only option before).

Baseline caveat (found 2026-07-10): gemma-3-4b-it F32 weights are NOT
staged on this host (only granite + bge are cached), so the "F32 vs Q4"
extractor A/B cannot run locally without a ~16 GB download + RSS spike.
Quality reference therefore comes from (a) the absolute ER-F1 ≥ 0.80 bar
and (b) A/B against the cloud-extraction LongMemEval run recorded
2026-07-10 — not from a local F32 twin. The F32 parity leg moves to
HUMAN-UAT hardware. GGUF artifacts land on /Volumes/Games (54 GiB free),
not the home volume.

## 4. Workstream B — reranker quant ladder

1. **Measure what we ship**: the O-01 per-device table
   (`docs/benchmarks/v0.4-O01-baselines.md`) is still all-TBD. Fill the
   Apple Silicon rows for FP32-Metal, Q5_K_M (CPU + Metal) with
   `cargo bench -p lunaris-bench --features metal --bench per_device`.
   A quant decision without the FP32 baseline number is guesswork.
2. **Produce Q4_K_M-imatrix** via the existing
   `spike-imatrix-bge-rerank.sh` + conversion script; pin SHA-256.
3. **Parity-gate Q4 vs Q5 vs FP32** on the reference-score harness
   (`scripts/spike-generate-reference-rerank-scores.py` set): pairwise
   order-inversion rate and |Δsigmoid| p95 (§5 thresholds).
4. **Decision**: if Q4 passes parity → new recommended tier (smaller,
   faster, same order quality); if not → Q5 stays, documented with data.
   Either way the winning tier becomes the **documented default recipe**
   (SDK `RerankerConfig` docs + book), honoring the runtime-toggle
   contract (consumers flip FP32↔quant via `with_reranker` at runtime —
   no admin reload endpoint).

## 4b. Workstream C — inference microscope (why is candle slow?)

The user-visible symptom is "embedder/reranker too slow", but we have no
per-stage attribution. Before (or alongside) swapping runtimes, instrument
the native paths so the bottleneck is a measurement, not a guess:

- `tracing` spans inside `NativeEmbedder`/`NativeReranker` hot paths:
  tokenize, pad/batch shape, forward, pooling/sigmoid, detach/copy —
  span fields carry batch size + max seq len (padding waste is a classic
  silent 2-4× tax for cross-encoders).
- A `profile_inference` example bin that replays a fixed corpus and prints
  the span histogram per stage, per device (CPU vs Metal), per quant tier.
- Output: a table attributing p50 latency to stages. Decision rule: if
  ≥60% of time is inside candle matmul kernels → runtime swap (Workstream
  D) is the lever; if it's padding/batching/tokenization → fix in place
  and keep candle.

## 4c. Workstream D — llama.cpp low-level runtime (spike MEASURED 2026-07-10)

llama.cpp (brew, build f5525f7e7, Metal) runs **all three of our GGUF
artifacts unmodified** — expected, since our conversion scripts produced
them with llama.cpp tooling; the granite ModernBERT arch that once needed
a PR patch is now upstream:

| Model (our artifact) | llama.cpp Metal throughput |
|---|---|
| granite-embedding-311m Q4_K_M | **13,650 tok/s** prefill (pp512) |
| bge-reranker-v2-m3 Q5_K_M | **5,731 tok/s** prefill (pp512; cross-encoder fwd = prefill) |
| gemma-3-4b-it QAT Q4_0 | **789 tok/s** prefill / **58.8 tok/s** decode (tg64) |

Interpretation:
- Extractor: ~59 tok/s decode makes local structured extraction genuinely
  viable (a 512-token chunk + ~300-token JSON answer ≈ 6 s, vs the F32/CPU
  path where the whole idea was abandoned for cloud APIs). This is the
  ceiling Workstream A's candle `quantized_gemma3` backend must be judged
  against — candle quantized Metal typically lands at 50-80% of llama.cpp;
  if it measures well below, D becomes the extractor runtime.
- Embedder/reranker: these are the kernel-level ceilings for Workstream C's
  decision rule.

Integration options, in preference order:
1. **`llama-cpp-2` crate (in-process FFI, static-linked)** — keeps the
   single-binary story; new `lunaris-llamacpp` backend crate behind an
   opt-in feature, implementing the same `Embedder`/`Reranker`/backend
   traits. C++ build dep is the cost (mirrors what Moon-style internal
   deps already tolerate in CI).
2. **`llama-server` subprocess** (OpenAI-compatible HTTP: /v1/embeddings,
   /v1/rerank, /v1/chat/completions) — near-zero Rust code because the
   Ollama/cloud HTTP backends already exist, but it reverses the v0.4 N-03
   "no external inference process in the supported path" cutover; operator
   escape hatch at most.
- Governance note: v0.4 N-03 consolidated on candle as THE runtime.
  Adopting llama.cpp even as opt-in is a deliberate partial reversal and
  gets its own ADR; the quality gates (§5) apply identically since the
  GGUF artifacts are byte-identical across runtimes.

## 5. Quality + perf gates (data-scientist view)

Red/green TDD: each gate lands as a failing test/bench first.

| Gate | Harness | Threshold |
|---|---|---|
| Reranker score parity | `quantized_equivalence.rs` pattern vs pinned FP32 reference scores | pairwise order-inversion ≤ 1% on reference set; |Δscore| p95 ≤ 0.02 (sigmoid space) |
| Extractor structural validity | GBNF/JSON parse-success rate over a fixed 100-chunk corpus | Q4 parse-success ≥ F32 − 1 pt |
| Extractor extraction quality | ER-F1 gauntlet (`lunaris-evals er-f1`) A/B F32 vs Q4 | Q4 F1 ≥ 0.80 bar AND ≥ F32 − 0.02 |
| End-to-end memory quality | 3-question graph A/B (q85/q87/q91 recipe), then N=50 LongMemEval with `LUNARIS_EVAL_LME_GRAPH=1` using the **native Q4 extractor** instead of cloud | J-score within noise of the cloud-extraction run recorded 2026-07-10 |
| Extractor perf | new `extractor_decode` per-device bench (tok/s, prefill ms, RSS) | ≥5× tok/s vs F32-CPU; RSS ≤ 3 GB |
| Reranker perf | per_device bench K=10 | Metal p50 ≤ 40 ms gate (O-01 table); Q4 ≥ 1.2× Q5 throughput to justify the tier |

## 6. Rollout + failure design

- Everything opt-in first (`extractor-gguf` feature + env), defaults
  flipped only after all §5 gates are green in CI-visible artifacts.
- Fail-open ladder unchanged: GGUF missing/corrupt → warn + F32 path →
  weights missing → Noop. A quantization rollout must never turn a
  working deployment into a hard failure (same contract as
  `resolve_embedder`'s GGUF fallback today).
- Rollback = unset one env var / disable one feature; no data migration,
  no index rebuild (quantization here is weights-only — Moon-side vector
  SQ8 is a separate, already-shipped concern).
- Every artifact SHA-256-pinned; `stage_models` bin extended to fetch the
  extractor GGUF so CI and dev boxes stage identically.

## 7. Explicitly out of scope (recorded, not forgotten)

- Verifier (gemma-3-270m/27b) quantization — same recipe applies; do it
  after the extractor proves the `QuantizedCandleBackend` seam.
- Embedder further quantization (already has Q4_K_M tier + parity tests).
- Speculative decoding / KV-cache quantization / batching-scheduler work —
  real levers, different milestone.
- Moon-side SQ8 ADC (shipped in `feat/moon-v051-perf-exploit`).

## 8. Sequencing

1. A1: `QuantizedCandleBackend` + red equivalence test (no weights yet).
   [IN PROGRESS 2026-07-10 — agent on feat/moon-v051-perf-exploit]
2. A2: stage QAT Q4_0 GGUF [DONE — /Volumes/Games/tindang-repo/models/],
   green the forward pass, decode bench vs the D-spike 58.8 tok/s ceiling.
3. C1: tracing microscope + profile_inference corpus replay (CPU/Metal ×
   FP-vs-quant table) → decision rule for D.
4. B1: fill O-01 FP32/Q5 baseline rows (blocked on benchmark completion —
   Metal contention; then `cargo bench --features metal --bench per_device`).
5. B2: build + parity-gate Q4_K_M-imatrix reranker.
6. D1 (conditional on C1): `lunaris-llamacpp` opt-in backend via
   `llama-cpp-2` + ADR for the partial N-03 reversal.
7. A3/B3: gauntlet A/Bs (ER-F1, graph A/B, N=50 LME) → flip defaults.
8. Docs: O-01 table, ARCHITECTURE.md model table, book model chapter.

Constraint active while the 2026-07-10 N=50 graph benchmark is running:
no rebuild of `target/release/lunaris-evals` (binary-swap mid-run would
corrupt the A/B); implementation compiles via `cargo check`/debug tests
only until that run completes.
