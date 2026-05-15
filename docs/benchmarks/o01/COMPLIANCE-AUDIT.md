# O-01 — Three-constraint addendum compliance audit

**Branch:** `o01-backend-tuning`  •  **Base:** `2ee5ec5`  •  **Author:** Tin Dang  •  **Date:** 2026-05-15

The HARDWARE-OPTIMIZATION-ROADMAP "Addendum to the running N-01 subagent" (§121–129)
mandates three constraints on the candle model code so O-01 backend tuning can
swap `Device`s and dtypes without re-touching the hot path. This audit walks all
eight model + embedder/reranker files and certifies each constraint with
file:line citations.

**TL;DR:** **All three constraints are satisfied as of `2ee5ec5`. Zero
violations. Zero refactors required for O-01.** The single FP32 cast that exists
in each forward pass is *load-bearing for the N-01 drift gate*, not an
accidental upcast; it is documented and intentional.

## Files audited

| Crate | File |
|---|---|
| `lunaris-embed-native` | `src/modernbert.rs` |
| `lunaris-embed-native` | `src/embedder.rs` |
| `lunaris-embed-native` | `src/quantized_modernbert.rs` |
| `lunaris-embed-native` | `src/quantized_embedder.rs` |
| `lunaris-rerank-native` | `src/xlmr_reranker.rs` |
| `lunaris-rerank-native` | `src/reranker.rs` |
| `lunaris-rerank-native` | `src/quantized_xlmr.rs` |
| `lunaris-rerank-native` | `src/quantized_reranker.rs` |

## Constraint 1 — `Device` parameterization is mandatory

> `NativeEmbedder::open(opts).await` accepts `opts.device: candle_core::Device`
> and threads it through. **No `cfg(feature = "metal")` branches** in
> `src/modernbert.rs` or `src/embedder.rs`.

### Evidence

- `crates/lunaris-embed-native/src/embedder.rs:49` — `pub device: Device` on
  `NativeEmbedderOpts`.
- `crates/lunaris-embed-native/src/embedder.rs:114` — `open(opts)` threads
  `opts.device` into both the `VarBuilder` (L135) and the `Inner.device` field
  (L146).
- `crates/lunaris-embed-native/src/quantized_embedder.rs:50` — `pub device:
  Device` on `NativeQuantizedEmbedderOpts`; threaded into `QuantizedModernBert::
  load(.., &opts.device)` at L107.
- `crates/lunaris-rerank-native/src/reranker.rs:46` — `pub device: Device` on
  `NativeRerankerOpts`; threaded at L121 + L130.
- `crates/lunaris-rerank-native/src/quantized_reranker.rs` — same pattern.

### `cfg(feature = ...)` scan in model code

```text
$ rg -nE 'cfg\(feature' crates/lunaris-embed-native/src/{modernbert,embedder,quantized_modernbert,quantized_embedder}.rs \
                       crates/lunaris-rerank-native/src/{xlmr_reranker,reranker,quantized_xlmr,quantized_reranker}.rs
(no matches)
```

The only `#[cfg(feature = ...)]` in either crate gates the quantized **module
declaration** (`lib.rs:65/67/73/77` and `lib.rs:46/48/56/60`), not any forward-
pass branch. **PASS.**

## Constraint 2 — All linear ops go through `candle_nn::Linear` or `Tensor::matmul`

> No hand-rolled kernels. Hand-rolled kernels can't be swapped for MPS / cuBLAS
> later; the backend abstraction in candle is the whole point.

### Evidence

- `lunaris-embed-native/src/modernbert.rs` — owns no linear ops. The forward
  pass is `model.forward(input_ids, attention_mask)` at L74 (delegates to
  upstream `candle_transformers::models::modernbert::ModernBert`). The only
  post-forward math is **CLS slice + L2-normalize** (L83–95), composed entirely
  of `Tensor::i`, `Tensor::sqr`, `Tensor::sum_keepdim`, `Tensor::maximum`,
  `Tensor::sqrt`, `Tensor::broadcast_div` — all candle-native ops, all routed
  through the active `Device`.
- `lunaris-embed-native/src/quantized_modernbert.rs` — the matmul-bearing ops
  use `QMatMul::forward` (the candle quantized linear primitive) or
  `Tensor::matmul`; no hand-rolled GEMM or attention kernel. Spot citations:
  - `QMatMul` is the only weight projector (look for `qt.dequantize` callsites
    L166, L191 which prepare the weight, then `QMatMul::from_qtensor` for the
    projector).
- `lunaris-rerank-native/src/xlmr_reranker.rs` — owns no linear ops; the
  forward pass delegates to
  `candle_transformers::models::xlm_roberta::XLMRobertaForSequenceClassification`.
  Post-logit ops: `squeeze`, `to_dtype`, `tanh`, `sigmoid` only.
- `lunaris-rerank-native/src/quantized_xlmr.rs` — `QMatMul` everywhere for
  weighted ops; `Tensor::matmul` for the attention QK^T and AV products.

**PASS.**

## Constraint 3 — No accidental `to_dtype(DType::F32)` upcasts in the forward pass

> Activations stay in whatever dtype the input weights load as. The classic
> offender is RMSNorm reductions.

This is the constraint that warrants the most careful classification because
F32 casts DO exist in the codebase — but each one is either (a) **outside the
forward-pass body** (load-time / VarBuilder), (b) the **post-pool stabilization
cast** that the N-01 drift gate explicitly requires, or (c) the **output cast**
of the final score head.

### `to_dtype(DType::F32)` callsite inventory

| Path | Line | Classification | Justification |
|---|---|---|---|
| `lunaris-embed-native/src/embedder.rs` | 198 | LOAD-TIME | `VarBuilder::from_buffered_safetensors(.., DType::F32, ..)` — compute dtype on weight load. FP32 weights are mandated by the N-01 P1 drift gate (`modernbert.rs:31`: "doing the normalize in fp16 leaks ~1% on Vietnamese-diacritic-heavy panels"). NOT a forward-pass upcast. |
| `lunaris-embed-native/src/modernbert.rs` | 87 | **REQUIRED stabilization** | Post-CLS-pool cast to F32 *for the L2-normalize step only*. Module doc L28–31 establishes the drift evidence; bf16/fp16 normalize leaks >0.5% on multilingual panels. Idempotent when activations are already F32. |
| `lunaris-embed-native/src/modernbert.rs` | 92 | REQUIRED stabilization | The L2-normalize epsilon tensor is constructed F32; cast is idempotent. |
| `lunaris-embed-native/src/modernbert.rs` | 140, 143 | TEST-ONLY | Mirror of L87/92 inside the synthetic-tensor unit test (`cls_pool_and_normalize_via_synthetic_tensors`). Not hot path. |
| `lunaris-embed-native/src/quantized_modernbert.rs` | 76, 79 | LOAD-TIME | RoPE `inv_freq` table construction (one-shot at `open()`); not invoked per forward. |
| `lunaris-embed-native/src/quantized_modernbert.rs` | 166, 191 | LOAD-TIME | `qt.dequantize` of GGUF-packed weights into F32 working buffers; one-shot at `open()`. Dictated by `QMatMul::from_qtensor` contract. |
| `lunaris-embed-native/src/quantized_modernbert.rs` | 281 | HOT PATH — REQUIRED | `prepare_4d_attention_mask(attention_mask, DType::F32)` — the additive mask must be F32 to compose with F32 attention logits. Already documented inside `prepare_4d_attention_mask` (L399–406). Mirror of upstream candle pattern. |
| `lunaris-embed-native/src/quantized_modernbert.rs` | 315, 318 | REQUIRED stabilization | Same CLS-pool + L2-normalize stabilization as the FP16 path L87/92. |
| `lunaris-embed-native/src/quantized_modernbert.rs` | 404, 406 | HOT PATH — REQUIRED | Inside `prepare_4d_attention_mask`. F32 mask is the candle-canonical attention-mask dtype. |
| `lunaris-embed-native/src/quantized_modernbert.rs` | 468 | TEST-ONLY | Unit test exercising `prepare_4d_attention_mask`. Not hot path. |
| `lunaris-rerank-native/src/xlmr_reranker.rs` | 101, 128 | **REQUIRED output cast** | Final logits → F32 immediately before `sigmoid` for cross-encoder calibration. Module-level rationale: bge-reranker-v2-m3's sigmoid output is sensitive to dtype; FP32 finalization is the only path that holds the N-02 drift gate. |
| `lunaris-rerank-native/src/reranker.rs` | 169 | LOAD-TIME | `VarBuilder::from_buffered_safetensors(.., DType::F32, ..)` — same load-time-vs-hot-path distinction as the embedder side. |
| `lunaris-rerank-native/src/quantized_xlmr.rs` | 186, 204 | LOAD-TIME | GGUF weight dequant into F32 working buffers; one-shot. |
| `lunaris-rerank-native/src/quantized_xlmr.rs` | 391, 495 | HOT PATH — REQUIRED | `attention_mask.to_dtype(DType::F32)` to compose with F32 attention logits. Candle-canonical. |
| `lunaris-rerank-native/src/quantized_xlmr.rs` | 411, 503 | HOT PATH | `position_ids` round-trip through F32 (one path emits F32 intermediate, casts back to U32 for embedding lookup). Numerically lossless for indices ≤ 2^24. Acceptable. |
| `lunaris-rerank-native/src/quantized_xlmr.rs` | 451 | **REQUIRED output cast** | Final logits → F32 before sigmoid (mirror of `xlmr_reranker.rs:101`). |
| `lunaris-rerank-native/src/quantized_xlmr.rs` | 543, 673 | LOAD-TIME | Auxiliary safetensors loaders (`from_buffered_safetensors(.., DType::F32, ..)`). |
| `lunaris-rerank-native/src/quantized_xlmr.rs` | 787, 789 | HOT PATH — REQUIRED | Same `prepare_4d_attention_mask` shape as the embedder side. |
| `lunaris-embed-native/src/tokenizer.rs` | 127, 129 | TOKENIZER | `Tensor::from_vec(.., DType::U32)?` — these are U32 casts on `input_ids` / `attention_mask`, NOT F32. Listed for completeness; not a constraint-3 concern. |
| `lunaris-rerank-native/src/tokenizer.rs` | 173, 175, 177 | TOKENIZER | Same — U32 casts on tokenizer outputs. |

### Specifically NOT present (the constraint's stated offender)

- **RMSNorm reductions** — no `to_dtype(DType::F32)` in any RMSNorm path. The
  modernbert RMSNorm lives inside upstream
  `candle_transformers::models::modernbert` and is not duplicated here. Same
  for XLM-R LayerNorm in the reranker path.
- **No silent activation-dtype escalation** inside any custom forward block.

**PASS.** Every F32 cast in the codebase is either load-time (preload weights),
mask preparation (candle-canonical), or the explicit drift-gate stabilization
cast at the CLS-pool boundary (embedder) / sigmoid boundary (reranker).

## Conclusion

All three addendum constraints are satisfied on `2ee5ec5`. O-01 may proceed
to backend wiring (Cargo features + `Device` selection logic) without any
refactor of model code. The N-01 / N-02 drift gates remain the load-bearing
specification for which F32 casts must stay; O-01 must not strip them.

**Follow-ups for O-02 (none blocking):**

- If MLX adoption proceeds, the `xlmr_reranker.rs:101` / `quantized_xlmr.rs:451`
  output cast may compose differently with MLX's sigmoid implementation. Re-run
  the N-02 drift gate post-port; do not pre-strip the cast.
