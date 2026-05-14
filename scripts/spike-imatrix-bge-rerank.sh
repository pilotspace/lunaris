#!/usr/bin/env bash
# scripts/spike-imatrix-bge-rerank.sh
#
# Re-quantize bge-reranker-v2-m3 GGUF using llama.cpp imatrix calibration,
# targeting the ≤ 0.10 sigmoid-space drift gate enforced by
# `crates/lunaris-rerank-native/tests/quantized_equivalence.rs`.
#
# Why this exists (recap from N-02 step 2):
# - Plain Q4_K_M (240 MiB, 4.53 BPW) fails the gate at max-delta 0.218
#   on rare-token Vietnamese / cross-lingual pairs.
# - The forward arithmetic in `QuantizedXlmRoberta` is bit-exact correct
#   against candle's reference (proven via `load_from_safetensors` in the
#   `quantized_layerwise_diag` test on pair #47, rel-L2 = 0.0).
# - The remediation is therefore quantization quality: imatrix-calibrated
#   Q5_K_M.
#
# Pipeline:
#   1. Start from the F16 GGUF produced by `spike-convert-bge-reranker-to-gguf.sh`
#   2. Patch metadata: flip `tokenizer.ggml.add_eos_token` to False so
#      `llama-imatrix` doesn't double-EOS during chunking. This flag is
#      cosmetic for Lunaris (we use the safetensors-side `tokenizer.json`
#      via `BGE_RERANKER_TOKENIZER_PATH`); the patched FP16 GGUF only
#      passes through `llama-imatrix` / `llama-quantize`.
#   3. Build llama-imatrix.
#   4. Run imatrix calibration on the curated dataset (200-500 lines mixing
#      EN, VI, code, cross-lingual, rare-tokens). Default lives at
#      $LUNARIS_SPIKE_CACHE/calibration/bge-rerank-calib.txt — produced
#      out-of-band; the script fails fast if it's missing.
#   5. Re-quantize FP16 → Q5_K_M with the imatrix.
#   6. SHA-256 + manifest.
#
# Drift gate result with this recipe (recorded 2026-05-14):
#   max sigmoid-space delta = 0.0425  (gate ≤ 0.10)
#   mean delta              = 0.00421 (gate ≤ 0.04)
#   GGUF size               = 446 MiB / 6.50 BPW
#   SHA-256                 = 6cdcc566200dba69553a89a9d59ff6d631e33969bc9367eff6914919f7722a1c

set -euo pipefail

LUNARIS_SPIKE_CACHE="${LUNARIS_SPIKE_CACHE:-$HOME/.cache/lunaris/spike}"
LLAMACPP_ROOT="$LUNARIS_SPIKE_CACHE/llamacpp/llama.cpp"
GGUF_DIR="$LUNARIS_SPIKE_CACHE/bge-reranker-v2-m3/gguf"
F16_GGUF="$GGUF_DIR/bge-reranker-v2-m3-f16.gguf"
F16_NOEOS="$GGUF_DIR/bge-reranker-v2-m3-f16-noeos.gguf"
Q5_GGUF="$GGUF_DIR/bge-reranker-v2-m3-Q5_K_M-imatrix.gguf"
CALIB_DIR="$LUNARIS_SPIKE_CACHE/calibration"
CALIB_TXT="$CALIB_DIR/bge-rerank-calib.txt"
IMATRIX="$CALIB_DIR/bge-rerank.imatrix"
SHA_FILE="$GGUF_DIR/Q4_K_M.sha256"

log() { printf '[spike-imatrix] %s\n' "$*" >&2; }
fail() { printf '[spike-imatrix][ERROR] %s\n' "$*" >&2; exit 1; }

[ -f "$F16_GGUF" ] || fail "F16 GGUF missing — run spike-convert-bge-reranker-to-gguf.sh first"
[ -f "$CALIB_TXT" ] || fail "calibration dataset missing at $CALIB_TXT
Curate ~500 lines with composition: 30% EN nat-lang, 25% VI, 20% code,
15% cross-lingual EN<->VI/code with </s></s> separator, 10% rare-tokens
(CJK fallthrough, emoji, math symbols, URLs)."

QUANT_BIN="$LLAMACPP_ROOT/build/bin/llama-quantize"
IMATRIX_BIN="$LLAMACPP_ROOT/build/bin/llama-imatrix"

# --- step 1: build llama-imatrix --------------------------------------------
if [ ! -x "$IMATRIX_BIN" ]; then
    log "building llama-imatrix"
    cmake --build "$LLAMACPP_ROOT/build" --target llama-imatrix -j
fi
[ -x "$QUANT_BIN" ] || fail "llama-quantize missing — run spike-convert-bge-reranker-to-gguf.sh first"

# --- step 2: patch FP16 GGUF: flip add_eos_token to False -------------------
# `llama-imatrix` asserts !add_eos because it appends EOS per chunk internally
# (would otherwise double-EOS). We need a metadata-only copy that preserves
# all numeric array types (notably tokenizer.ggml.precompiled_charsmap as
# UINT8 — promoting it to INT32 breaks downstream vocab loading).
#
# `gguf-py`'s `copy_with_new_metadata` does this safely (preserves source
# array element types).
if [ ! -f "$F16_NOEOS" ]; then
    log "patching FP16 GGUF: tokenizer.ggml.add_eos_token = False → $F16_NOEOS"
    VENV="$LLAMACPP_ROOT/.venv"
    [ -x "$VENV/bin/python" ] || fail "converter venv missing at $VENV — re-run spike-convert-bge-reranker-to-gguf.sh"
    "$VENV/bin/python" - <<PY
import sys
from pathlib import Path
LLAMA = Path("$LLAMACPP_ROOT")
sys.path.insert(0, str(LLAMA / "gguf-py/gguf/scripts"))
sys.path.insert(0, str(LLAMA / "gguf-py"))
import gguf
from gguf_new_metadata import MetadataDetails, copy_with_new_metadata
reader = gguf.GGUFReader("$F16_GGUF", 'r')
arch_field = reader.fields['general.architecture']
arch = bytes(arch_field.parts[arch_field.data[0]]).decode()
writer = gguf.GGUFWriter("$F16_NOEOS", arch=arch, endianess=reader.endianess)
new_meta = {"tokenizer.ggml.add_eos_token": MetadataDetails(gguf.GGUFValueType.BOOL, False)}
copy_with_new_metadata(reader, writer, new_meta, [])
PY
else
    log "noeos FP16 GGUF cached: $F16_NOEOS"
fi

# --- step 3: imatrix calibration --------------------------------------------
if [ ! -f "$IMATRIX" ]; then
    log "running llama-imatrix on $CALIB_TXT → $IMATRIX"
    # -c 512 / -ub 512 / -b 512: pin context-physical-batch trio so the
    # encoder's `n_ubatch >= n_tokens` assert holds (XLM-R is encoder-only).
    # --process-output: include the output projection in the imatrix.
    "$IMATRIX_BIN" \
        -m "$F16_NOEOS" \
        -f "$CALIB_TXT" \
        -o "$IMATRIX" \
        --output-format gguf \
        -c 512 -ub 512 -b 512 \
        --process-output
else
    log "imatrix cached: $IMATRIX"
fi

# --- step 4: Q5_K_M with imatrix --------------------------------------------
if [ ! -f "$Q5_GGUF" ]; then
    log "quantizing FP16 → Q5_K_M with imatrix: $Q5_GGUF"
    "$QUANT_BIN" --imatrix "$IMATRIX" "$F16_NOEOS" "$Q5_GGUF" Q5_K_M
else
    log "Q5_K_M imatrix GGUF cached: $Q5_GGUF"
fi

# --- step 5: SHA + manifest --------------------------------------------------
if command -v sha256sum >/dev/null 2>&1; then
    SHA256_BIN="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    SHA256_BIN="shasum -a 256"
else
    fail "neither sha256sum nor shasum found"
fi
SHA256_HEX="$( $SHA256_BIN "$Q5_GGUF" | awk '{print $1}' )"
echo "$SHA256_HEX  $(basename "$Q5_GGUF")" > "$SHA_FILE"
log "SHA-256 = $SHA256_HEX"
log "pin in crates/lunaris-rerank-native/src/lib.rs:"
log "    pub const BGE_RERANKER_GGUF_Q4_SHA256: &str = \"$SHA256_HEX\";"

size_bytes="$(stat -f '%z' "$Q5_GGUF" 2>/dev/null || stat -c '%s' "$Q5_GGUF")"
size_mib=$(( size_bytes / 1024 / 1024 ))
log "Q5_K_M (imatrix) size: ${size_mib} MiB ($size_bytes bytes)"
log "done."
log "GGUF: $Q5_GGUF"
echo "$SHA256_HEX"
