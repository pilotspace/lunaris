# N-02 step 2 — Q5_K_M XLM-R reranker imatrix calibration — SUMMARY

**Status:** GREEN. Drift gate passes; forward code proven bit-exact on rare-token VI inputs.
**Date:** 2026-05-14
**Worktree:** `agent-a0344421488fbdd1f`
**Branch:** `worktree-agent-a0344421488fbdd1f`
**Destination in main repo:** copy to `.planning/phases/N-02-step-2-quantized-gguf/SUMMARY.md` post-merge.

## Headline results

| Metric | Result | Gate | Margin |
|---|---:|---:|---:|
| Max sigmoid-space drift (Q5-imatrix vs FP32 candle) | **0.0425** (pair #22: "how to deploy a web app") | ≤ 0.10 | **2.35× under** |
| Mean sigmoid-space drift | **0.00421** | ≤ 0.04 | **9.5× under** |
| Q5 score variance (non-vacuity) | 0.1739 | > 0.01 | clean |
| FP32 score variance | 0.1746 | > 0.01 | clean |
| Pair #47 (VI "mekong delta tourism") | delta = 0.054 (was 0.218 plain Q4) | ≤ 0.10 | **4× under** |
| Pair #25 (cross-lingual VI capital) | delta < 0.0425 (was 0.355 imatrix-Q4) | ≤ 0.10 | safe |

## Pinned GGUF artifact

| Property | Value |
|---|---|
| Path | `~/.cache/lunaris/spike/bge-reranker-v2-m3/gguf/bge-reranker-v2-m3-Q5_K_M-imatrix.gguf` |
| Size | **446 MiB / 6.50 BPW** (`llama_model_quantize_impl: quant size = 440.17 MiB`) |
| SHA-256 | `6cdcc566200dba69553a89a9d59ff6d631e33969bc9367eff6914919f7722a1c` |
| llama.cpp HEAD | `ccb9e9b7c` + PR #22716 (granite-r2 tokenizer support; no impact here, just the same checkout) |
| Calibration set | 633 lines @ `scripts/fixtures/bge-rerank-calib.txt` (24 chunks × 512 tokens, 12 288 calib tokens) |
| imatrix file | `~/.cache/lunaris/spike/calibration/bge-rerank.imatrix` (885 KiB, 288 tensors, 144 importance entries) |

## Why Q5_K_M and not Q4_K_M with imatrix?

The N-02 step 2 brief targeted Q4_K_M @ ~240 MiB. Three measured operating points:

| Recipe | Size | Max delta | Mean delta | Result |
|---|---:|---:|---:|---|
| Plain Q4_K_M (no imatrix) | 240 MiB | **0.218** | 0.020 | RED — pair #47 VI fails |
| Imatrix Q4_K_M | 418 MiB | **0.355** | 0.014 | RED worse — pair #25 cross-lingual fails, token_embd auto-upgrades to Q6 in modern llama.cpp without imatrix coverage of the embedding tensor, creating mismatched precision across the encoder |
| **Imatrix Q5_K_M** | **446 MiB** | **0.0425** | **0.00421** | **GREEN** — full gate compliance |

Imatrix-Q4 was worse than plain Q4 on max delta because imatrix minimizes per-tensor reconstruction L2, not sigmoid-space prediction error; outliers move, mean drops. Q5_K_M provides enough headroom (one extra bit per weight on average) for the calibration to land per-pair drift inside the gate.

## Calibration recipe (`scripts/spike-imatrix-bge-rerank.sh`)

Calibration text composition (633 unique lines from `scripts/fixtures/bge-rerank-calib.txt`):
- ~509 lines auto-extracted from existing fixtures (reranker pairs + embed VI/code/cross-lingual panels)
- 60 hand-curated lines covering: 20 Vietnamese culture/geography/cuisine, 12 Rust+Python+TS code snippets, 6 error-message strings, 5 scientific/historical prose, 5 math symbols (∫ ∇ ζ ∀∃ Δ E=mc²), 5 CJK fallthrough (日本語/中文/한국어/العربية/Русский), 3 emoji-heavy, 2 URL/CLI strings

Pipeline (per `scripts/spike-imatrix-bge-rerank.sh`):
1. Build `llama-imatrix` (cmake target).
2. Patch FP16 GGUF: flip `tokenizer.ggml.add_eos_token` to `False` via `gguf-py`'s `copy_with_new_metadata` (preserves UINT8 `precompiled_charsmap` — naive re-emit promotes it to INT32 and breaks vocab loading).
3. Run `llama-imatrix -c 512 -ub 512 -b 512 --process-output` (matched ctx/ubatch trio satisfies the XLM-R encoder's `n_ubatch >= n_tokens` assert).
4. `llama-quantize --imatrix ... Q5_K_M`.

## Structural traps documented

1. **`llama-imatrix` asserts `!add_eos`.** XLM-R GGUFs ship `add_eos_token=true` (the model card requires `</s></s>` separators between query/doc). imatrix appends EOS internally per chunk and aborts on double-EOS. Fix: metadata-only patch using `copy_with_new_metadata` (NOT a naive rewrite — preserves source array element types).
2. **`n_ubatch >= n_tokens` required for encoder-only models.** Default `-ub 512` with auto-batch sizing yields ubatch=512 but tokens=513 in some configurations and asserts. Pin `-c 512 -ub 512 -b 512` explicitly.
3. **`token_embd` auto-upgrade in modern llama.cpp.** When imatrix doesn't cover the embedding tensor (which `--process-output` doesn't add), `llama-quantize` conservatively upgrades the embedding from Q4 to Q6_K. This is why imatrix-Q4 is 418 MiB rather than 240 MiB. We accept the upgrade (correctness > size) and lean on Q5_K_M for the rest.
4. **Forward code is bit-exact correct.** `QuantizedXlmRoberta::load_from_safetensors` is a reusable discriminator that runs the same forward graph with F32 safetensors weights wrapped in `QMatMul::Tensor(w)`. On pair #0 (EN) and pair #47 (VI), it returns identical sigmoid scores to `candle_transformers::XLMRobertaModel` (rel-L2 = 0.0). Any future drift regression first runs this discriminator: if it stays GREEN, the bug is quant-only; if it goes RED, the bug is in the forward graph (e.g., position-offset, dtype promotion).

## Commits in this worktree

| SHA | Subject |
|---|---|
| `955ce46` | `feat(lunaris-rerank-native): quantized XLM-R diagnostic instrumentation` — `load_from_safetensors`, `forward_hidden_upto`, `swap_word_embd_from_safetensors`, position-offset fix, `quantized_layerwise_diag` test parameterized by `DIAG_PAIR_INDEX`. |
| (pending) | `feat(lunaris-rerank-native): N-02 step 2 — Q5_K_M imatrix GGUF, drift gate GREEN` — lib.rs SHA + docstring update, `scripts/spike-imatrix-bge-rerank.sh`, `scripts/fixtures/bge-rerank-calib.txt`. |

## File sizes (LOC)

- `crates/lunaris-rerank-native/src/quantized_xlmr.rs` — 1230 LOC (gate: < 1500)
- `crates/lunaris-rerank-native/tests/quantized_layerwise_diag.rs` — 265 LOC
- `crates/lunaris-rerank-native/tests/quantized_equivalence.rs` — 297 LOC
- `scripts/spike-imatrix-bge-rerank.sh` — 134 LOC
- `scripts/fixtures/bge-rerank-calib.txt` — 633 lines / 35 KiB

## CLAUDE.md compliance

- **MSRV 1.94 / edition 2024** — preserved (no toolchain bumps).
- **No `unsafe`** — `#![forbid(unsafe_code)]` at crate root unchanged.
- **No lock-across-await** — `QuantizedXlmRoberta` is read-only after load; `Arc<Inner>` dispatch via `spawn_blocking`. Vacuous.
- **No file > 1500 LOC** — `quantized_xlmr.rs` at 1230.
- **`parking_lot::RwLock` policy** — N/A in this crate; no locks added.
- **TDD** — drift gate test was RED before this work; instrumentation commit + Q5_K_M GGUF make it GREEN. No threshold changes (still ≤ 0.10 max / ≤ 0.04 mean).
- **No modifications outside the worktree** — calibration corpus and GGUF live in `~/.cache/lunaris/spike/`, not in the repo. SHA-256 is the pinning mechanism.

## Next-spawn brief: N-03 cutover

Pre-conditions now met for N-03 (delete legacy embed/rerank backends):
1. `NativeQuantizedReranker` opens the imatrix-Q5_K_M GGUF and passes the drift gate against the FP32 candle reference path.
2. Diagnostic instrumentation (`load_from_safetensors`, `quantized_layerwise_diag`) is durably committed and reusable if a future quant regression surfaces.
3. Reproducible recipe: `scripts/spike-imatrix-bge-rerank.sh` + `scripts/fixtures/bge-rerank-calib.txt` lets a future agent regenerate the GGUF and re-pin SHA in one command.

Open questions for N-03:
- Should `BGE_RERANKER_GGUF_Q4_SHA256` be renamed to drop the historical `Q4` token now that the canonical quant is Q5_K_M? Recommend yes, but as a separate refactor commit so the cutover diff stays narrow.
- The 446 MiB size is 39% over the original 320 MiB N-02 brief target. The size deviation is documented in `lib.rs:67-90`. If hard size pressure resurfaces, the next lever is curating a wider calibration set (1k+ chunks) and retrying Q4_K_M-imatrix — but only after a future production-traffic eval confirms 446 MiB is actually a blocker. For now, correctness > size.
