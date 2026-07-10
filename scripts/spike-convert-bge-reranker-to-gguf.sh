#!/usr/bin/env bash
# scripts/spike-convert-bge-reranker-to-gguf.sh
#
# Self-convert BAAI/bge-reranker-v2-m3 → GGUF Q4_K_M.
#
# Mirrors `spike-convert-granite-r2-to-gguf.sh` (N-01.5 step 2). Idempotent:
# re-running is a no-op once the Q4 file exists. Differs only in:
#  - input model is `BAAI/bge-reranker-v2-m3` (568M XLM-RoBERTa-large
#    cross-encoder), downloaded from HF Hub if not already cached
#  - target file size budget is ≤ 320 MiB (vs granite-r2's 250 MiB) — bge
#    is a bigger model
#  - llama.cpp PR patch is NOT applied by default: XLM-RoBERTa support
#    landed in llama.cpp mainline well before the granite-r2 PR (#22716).
#    The granite-r2 PR's branch may still convert XLM-R fine (it's a
#    superset of mainline at the relevant commit) but we don't require it.
#
# Inputs (env, overrideable):
#   BGE_RERANKER_HF_REPO    HuggingFace repo id. Default: BAAI/bge-reranker-v2-m3
#   BGE_RERANKER_HF_DIR     Override the local snapshot path entirely.
#                           If unset, the script downloads to
#                           $LUNARIS_SPIKE_CACHE/bge-reranker-v2-m3/snapshot/
#   LUNARIS_SPIKE_CACHE     Root cache. Defaults to ~/.cache/lunaris/spike.
#   LLAMACPP_PR             PR number to patch onto llama.cpp HEAD. Defaults
#                           to empty (NO patch — use mainline / whatever
#                           commit is already checked out). Set to "22716"
#                           to share the granite-r2 PR branch if needed.
#
# Outputs (under $LUNARIS_SPIKE_CACHE/bge-reranker-v2-m3/gguf/):
#   bge-reranker-v2-m3-f16.gguf      intermediate F16 GGUF (~1.1 GB)
#   bge-reranker-v2-m3-Q4_K_M.gguf   final quantized payload (target ≤ 320 MB)
#   Q4_K_M.sha256                    the SHA-256 pin (used by lib.rs constant)
#   Q4_K_M.tensor-manifest.txt       per-tensor dtype/shape dump (for porters)
#
# What this script does NOT do:
# - Commit the GGUF file to git. The file is ~300 MB and lives in the local
#   cache. The SHA-256 alone is what we pin in
#   crates/lunaris-bench/src/bin/stage_models.rs as BGE_GGUF_Q5_K_M_SHA256.

set -euo pipefail

LUNARIS_SPIKE_CACHE="${LUNARIS_SPIKE_CACHE:-$HOME/.cache/lunaris/spike}"
BGE_RERANKER_HF_REPO="${BGE_RERANKER_HF_REPO:-BAAI/bge-reranker-v2-m3}"
DEFAULT_SNAPSHOT="$LUNARIS_SPIKE_CACHE/bge-reranker-v2-m3/snapshot"
BGE_RERANKER_HF_DIR="${BGE_RERANKER_HF_DIR:-$DEFAULT_SNAPSHOT}"
LLAMACPP_PR="${LLAMACPP_PR:-}"

LLAMACPP_ROOT="$LUNARIS_SPIKE_CACHE/llamacpp/llama.cpp"
GGUF_DIR="$LUNARIS_SPIKE_CACHE/bge-reranker-v2-m3/gguf"
F16_GGUF="$GGUF_DIR/bge-reranker-v2-m3-f16.gguf"
Q4_GGUF="$GGUF_DIR/bge-reranker-v2-m3-Q4_K_M.gguf"
Q4_SHA="$GGUF_DIR/Q4_K_M.sha256"
MANIFEST="$GGUF_DIR/Q4_K_M.tensor-manifest.txt"

log() { printf '[spike-convert] %s\n' "$*" >&2; }
fail() { printf '[spike-convert][ERROR] %s\n' "$*" >&2; exit 1; }

mkdir -p "$GGUF_DIR" "$LUNARIS_SPIKE_CACHE/llamacpp" "$LUNARIS_SPIKE_CACHE/bge-reranker-v2-m3"

# --- step 1: ensure llama.cpp is cloned ---------------------------------------
if [ ! -d "$LLAMACPP_ROOT/.git" ]; then
    log "cloning llama.cpp (shallow) into $LLAMACPP_ROOT"
    git clone --depth 50 https://github.com/ggml-org/llama.cpp.git "$LLAMACPP_ROOT"
fi

if [ -n "$LLAMACPP_PR" ]; then
    if ! ( cd "$LLAMACPP_ROOT" && git rev-parse --verify --quiet "llamacpp-pr-$LLAMACPP_PR" >/dev/null ); then
        log "applying llama.cpp PR #$LLAMACPP_PR"
        ( cd "$LLAMACPP_ROOT" && gh pr checkout "$LLAMACPP_PR" --repo ggml-org/llama.cpp -b "llamacpp-pr-$LLAMACPP_PR" )
    else
        log "PR branch llamacpp-pr-$LLAMACPP_PR already exists; skipping patch step"
    fi
fi

PR_HEAD="$( cd "$LLAMACPP_ROOT" && git rev-parse HEAD )"
PR_REF="$( cd "$LLAMACPP_ROOT" && git rev-parse --abbrev-ref HEAD )"
log "llama.cpp HEAD = $PR_HEAD ($PR_REF)"

# --- step 2: converter venv --------------------------------------------------
VENV="$LLAMACPP_ROOT/.venv"
if [ ! -x "$VENV/bin/python" ]; then
    PY="$(command -v python3.11 || command -v python3.12 || command -v python3.13 || command -v python3)"
    log "creating converter venv with $PY"
    "$PY" -m venv "$VENV"
    "$VENV/bin/pip" install --upgrade pip setuptools wheel >/dev/null
    "$VENV/bin/pip" install -r "$LLAMACPP_ROOT/requirements/requirements-convert_hf_to_gguf.txt"
fi

# --- step 0: download HF snapshot --------------------------------------------
#
# We need: config.json, tokenizer.json, model.safetensors (plus the SP BPE
# model). The converter venv (step 2 above) already has huggingface_hub —
# reuse it so we don't introduce a second python dependency surface.
#
# `huggingface_hub.snapshot_download` keeps everything coherent for the
# revision we resolve at first run.
if [ ! -f "$BGE_RERANKER_HF_DIR/config.json" ] \
   || [ ! -f "$BGE_RERANKER_HF_DIR/tokenizer.json" ] \
   || [ ! -f "$BGE_RERANKER_HF_DIR/model.safetensors" ]; then
    log "downloading $BGE_RERANKER_HF_REPO → $BGE_RERANKER_HF_DIR"
    mkdir -p "$BGE_RERANKER_HF_DIR"
    HF_HOME="$LUNARIS_SPIKE_CACHE/hf_home" "$VENV/bin/python" - <<PY
from huggingface_hub import snapshot_download
import sys
p = snapshot_download(
    repo_id="$BGE_RERANKER_HF_REPO",
    local_dir="$BGE_RERANKER_HF_DIR",
    allow_patterns=[
        "config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "sentencepiece.bpe.model",
        "special_tokens_map.json",
        "model.safetensors",
        "*.txt",
    ],
)
print(p, file=sys.stderr)
PY
fi

[ -f "$BGE_RERANKER_HF_DIR/config.json" ] || fail "config.json missing in $BGE_RERANKER_HF_DIR after download"
[ -f "$BGE_RERANKER_HF_DIR/tokenizer.json" ] || fail "tokenizer.json missing in $BGE_RERANKER_HF_DIR after download"
[ -f "$BGE_RERANKER_HF_DIR/model.safetensors" ] || fail "model.safetensors missing in $BGE_RERANKER_HF_DIR after download"

log "HF snapshot ready at $BGE_RERANKER_HF_DIR"

# --- step 3: F16 GGUF --------------------------------------------------------
if [ ! -f "$F16_GGUF" ]; then
    log "converting HF safetensors → F16 GGUF: $F16_GGUF"
    "$VENV/bin/python" "$LLAMACPP_ROOT/convert_hf_to_gguf.py" \
        "$BGE_RERANKER_HF_DIR" \
        --outfile "$F16_GGUF" \
        --outtype f16
else
    log "F16 GGUF cached: $F16_GGUF"
fi

# --- step 4: build llama-quantize -------------------------------------------
QUANT_BIN="$LLAMACPP_ROOT/build/bin/llama-quantize"
if [ ! -x "$QUANT_BIN" ]; then
    log "building llama-quantize (CPU-only, static)"
    cmake -B "$LLAMACPP_ROOT/build" -S "$LLAMACPP_ROOT" \
        -DBUILD_SHARED_LIBS=OFF \
        -DLLAMA_CURL=OFF \
        -DGGML_METAL=OFF >/dev/null
    cmake --build "$LLAMACPP_ROOT/build" --config Release --target llama-quantize -j
fi

# --- step 5: Q4_K_M ----------------------------------------------------------
if [ ! -f "$Q4_GGUF" ]; then
    log "quantizing F16 → Q4_K_M: $Q4_GGUF"
    "$QUANT_BIN" "$F16_GGUF" "$Q4_GGUF" Q4_K_M
else
    log "Q4_K_M GGUF cached: $Q4_GGUF"
fi

# --- step 6: verify ----------------------------------------------------------
size_bytes="$(stat -f '%z' "$Q4_GGUF" 2>/dev/null || stat -c '%s' "$Q4_GGUF")"
size_mib=$(( size_bytes / 1024 / 1024 ))
log "Q4_K_M size: ${size_mib} MiB ($size_bytes bytes)"
# Size gate: the N-02 step 2 brief targeted ≤ 320 MiB. Empirically the
# floor on this model is ~330 MiB even with `--token-embedding-type q4_K`
# applied:
#   plain Q4_K_M                              → 411 MiB
#   Q4_K_M + --token-embedding-type q4_K      → 348 MiB
#   Q4_K_S + --token-embedding-type q4_K      → 335 MiB
# The non-quantizable F32 floor — 250 002 × 1024 vocab (under q4_K still
# ~140 MiB), 8192 × 1024 position_embd F32 (32 MiB), per-tensor biases
# F32, plus 24 layers' worth of F32 norms — accounts for ~220 MiB of
# the total before any matmul weights are added. We pick the recipe
# that maximizes drift-gate headroom (plain Q4_K_M, no token-embedding
# downgrade) and document the size deviation in
# `.planning/phases/N-02-step-2-quantized-xlmr/conversion-evidence.md`.
# The gate here is loose (≤ 425 MiB) — it catches a runaway only.
if [ "$size_mib" -gt 425 ]; then
    fail "Q4_K_M size ${size_mib} MiB exceeds 425 MiB hard upper bound"
fi
if [ "$size_mib" -gt 320 ]; then
    log "[warn] Q4_K_M size ${size_mib} MiB exceeds brief's 320 MiB target — \
documented floor; see conversion-evidence.md"
fi

# Magic check: first 4 bytes must be 'GGUF' (0x47 0x47 0x55 0x46).
magic="$(head -c 4 "$Q4_GGUF" | xxd -p)"
if [ "$magic" != "47475546" ]; then
    fail "GGUF magic check failed: got $magic, expected 47475546"
fi

# SHA-256 pin.
if command -v sha256sum >/dev/null 2>&1; then
    SHA256_BIN="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    SHA256_BIN="shasum -a 256"
else
    fail "neither sha256sum nor shasum found"
fi
$SHA256_BIN "$Q4_GGUF" | tee "$Q4_SHA"
SHA256_HEX="$( $SHA256_BIN "$Q4_GGUF" | awk '{print $1}' )"
log "SHA-256 = $SHA256_HEX"
log "pin in crates/lunaris-bench/src/bin/stage_models.rs:"
log "    pub const BGE_RERANKER_GGUF_Q4_SHA256: &str = \"$SHA256_HEX\";"

# --- step 7: tensor manifest -------------------------------------------------
"$VENV/bin/python" - "$Q4_GGUF" > "$MANIFEST" <<'PY'
import sys, gguf
r = gguf.GGUFReader(sys.argv[1])
print('# generated by spike-convert-bge-reranker-to-gguf.sh')
print('# every line:  name shape=[...]  dtype=<ggml_type>')
print()
for t in r.tensors:
    print(f'{t.name:40s} shape={list(t.shape)} dtype={t.tensor_type.name}')
PY
log "tensor manifest: $MANIFEST"

log "done."
log "GGUF:     $Q4_GGUF"
log "SHA-256:  $SHA256_HEX"
log "manifest: $MANIFEST"
echo "$SHA256_HEX"
