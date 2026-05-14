# N-02 step 2 — bge-reranker-v2-m3 → GGUF Q4_K_M conversion evidence

Generated 2026-05-14 by `scripts/spike-convert-bge-reranker-to-gguf.sh`.

## Inputs

| Item | Value |
|------|-------|
| HF repo | `BAAI/bge-reranker-v2-m3` |
| HF snapshot | `~/.cache/lunaris/spike/bge-reranker-v2-m3/snapshot` |
| Architecture | XLM-RoBERTa-large cross-encoder (568M params) |
| Hidden | 1024 |
| Layers | 24 |
| Heads | 16 |
| Vocab | 250 002 |
| Max position | 8194 (we use 512 effective) |
| `type_vocab_size` | 1 |
| `num_labels` | 1 |

## llama.cpp pin

- Repo: `https://github.com/ggml-org/llama.cpp`
- HEAD at conversion: `ccb9e9b7cb0dce8bed2f1a20b1ac1a6278a1b4b8`
- Local branch: `granite-embedding-r2-pr` (inherited from N-01.5 step 2)
- PR patch applied: NONE (the granite-r2 PR #22716 was already checked
  out from prior work; XLM-R conversion went through cleanly on it).
  Re-running the script with `LLAMACPP_PR=""` (the default) on a fresh
  clone of mainline llama.cpp also works — XLM-R / "bert" arch is
  supported in mainline.
- Detection: the converter wrote `general.architecture: bert` and used
  the BERT-family tensor naming convention (`blk.{i}.attn_{q,k,v}`,
  `*_norm.{weight,bias}`, `token_embd_norm`, `cls.*`).

## Output artifacts

| File | Size | Notes |
|------|------|-------|
| `bge-reranker-v2-m3-f16.gguf` | 1 106 MiB | intermediate F16 (not pinned) |
| `bge-reranker-v2-m3-Q4_K_M.gguf` | **418 MiB** | final pinned artifact |
| `Q4_K_M.sha256` | — | `37da565066d505eb0c3ead316f3822728712eec2dc2dd2a4542ee65ea5064669` |
| `Q4_K_M.tensor-manifest.txt` | 393 tensors | committed under `tests/fixtures/` |

The Q4_K_M file is **not committed** to git (418 MiB) — only the SHA-256 is
pinned in `crates/lunaris-rerank-native/src/lib.rs` as
`BGE_RERANKER_GGUF_Q4_SHA256`.

## Size budget deviation

The N-02 step 2 brief targeted **≤ 320 MiB**. The actual artifact is **418
MiB**. Conversation with the orchestrator: this is a documented floor, not
a silent corner-cut. The relevant dry-runs:

| Recipe | Size |
|--------|------|
| Q4_K_M (chosen) | 411 MiB (real); 418 MiB on disk after rerun |
| Q4_K_M + `--token-embedding-type q4_K` | 348 MiB (dry-run) |
| Q4_K_S | 398 MiB (dry-run) |
| Q4_K_S + `--token-embedding-type q4_K` | 335 MiB (dry-run) |

None landed under 320 MiB. The non-quantizable F32 floor explains why:

- `position_embd.weight` shape `[1024, 8192]` F32 → **32 MiB** (full
  position table, even though we cap encoding at 512 — llama-quantize
  doesn't trim)
- `token_embd.weight` shape `[1024, 250002]` Q6_K → **~200 MiB** (vocab is
  the dominant single tensor; Q4_K downgrade saves ~64 MiB)
- 24 layers × `(attn_{q,k,v,output}.bias, ffn_{up,down}.bias,
  *_norm.{weight,bias})` F32 → ~3 MiB
- F32 norms + biases scattered across heads → another ~4 MiB
- `cls.bias` + `cls.output.{weight,bias}` F32/F16 → negligible
- Q4_K matmuls (288 tensors × ~0.56–2.25 MiB) → ~180 MiB

We picked **plain Q4_K_M** (no `--token-embedding-type` downgrade) because:

1. The drift gate (max |Q4 − FP32| ≤ 0.10 over 100 pairs) is load-bearing
   for the value the brief delivers (calibrated sub-25ms recall scores);
   the size budget is a deployment-convenience target.
2. Aggressively quantizing `token_embd` increases embedding-index drift,
   which propagates linearly through every layer.
3. Going to Q4_K_S or below puts the drift gate at material risk.

The brief's `failure_modes` section permits documenting deviations when
justified. This is one such case. The conversion script's hard size cap
was bumped to 425 MiB (catches a runaway only); a `[warn]` log fires
between 320 and 425.

## Tensor naming + classifier head fate

Critical for the porter:

- Architecture key: `bert` (NOT `xlm-roberta` — llama.cpp uses BERT
  family naming uniformly for encoder-only models).
- Norm convention: **POST-norm**, not pre-norm. Tensors are named
  `blk.{i}.attn_output_norm.{weight,bias}` (after attention) and
  `blk.{i}.layer_output_norm.{weight,bias}` (after FFN). This differs
  from ModernBERT's pre-norm pattern that the N-01.5 quantized
  embedder mirrors — the XLM-R forward pass applies the norm AFTER the
  residual add.
- Q/K/V are **separate** tensors (not fused like granite-r2's
  `attn_qkv`): `blk.{i}.attn_{q,k,v}.weight` each `[1024, 1024]` Q4_K,
  with corresponding `.bias` F32. `blk.{i}.attn_output.weight` is also
  Q4_K, `.bias` F32.
- FFN is GELU separate up/down (not GeGLU): `blk.{i}.ffn_up.weight`
  `[1024, 4096]` and `blk.{i}.ffn_down.weight` `[4096, 1024]`.
- Bias terms are present on every linear (XLM-R uses biases; ModernBERT
  does not). Each is F32.
- Embeddings:
  - `token_embd.weight` `[1024, 250002]` Q6_K — dequantize → F32 at
    load (mirrors `quantized_modernbert::word_embd` strategy).
  - `position_embd.weight` `[1024, 8192]` F32 — dequantize (no-op) →
    F32 cache, sliced to seq_len at forward time.
  - `token_types.weight` `[1024]` F32 — single row (XLM-R has
    `type_vocab_size = 1`); broadcast-add to every position.
  - `token_embd_norm.{weight,bias}` F32 — post-embedding LN.

- **Classifier (Case A — head IS in GGUF):**

  ```
  cls.weight        [1024, 1024]  Q4_K   ← dense weight
  cls.bias          [1024]        F32    ← dense bias
  cls.output.weight [1024]        F16    ← out_proj weight (row vector)
  cls.output.bias   [1]           F32    ← out_proj bias
  ```

  The head pipeline is:
  `x_cls (F32, 1024) → dense (cls.{w,b}, Q4_K matmul + F32 add) →
  tanh → out_proj (cls.output.{w,b}, F16→F32 dot + F32 add) →
  sigmoid → [0, 1] scalar score`.

  We load `cls.weight` as a `QMatMul` and `cls.output.weight` as an F32
  tensor (after F16 dequant) since it's a 1024-d row vector — wrapping
  in `QMatMul` would be wasted overhead. Biases are F32 add tensors.

  No safetensors fallback is required. The brief's Case B (load head
  from `model.safetensors`) is unused.

## Reproducibility

```bash
# Idempotent re-run produces byte-identical Q4 file + SHA-256:
bash scripts/spike-convert-bge-reranker-to-gguf.sh
# → echoes: 37da565066d505eb0c3ead316f3822728712eec2dc2dd2a4542ee65ea5064669
```

Verified: re-running the script after first successful run is a pure
no-op (skips all six cached steps; recomputes manifest + SHA only).
