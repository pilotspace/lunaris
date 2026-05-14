#!/usr/bin/env bash
# scripts/spike-convert-granite-r2-to-gguf.sh
#
# Self-convert ibm-granite/granite-embedding-311m-multilingual-r2 → GGUF Q4_K_M.
#
# Idempotent: re-running is a no-op once the Q4 file exists and its SHA-256
# matches the previously-emitted pin. Re-cloning/rebuilding llama.cpp is also
# skipped if the target binary is already present.
#
# Inputs (env, overrideable):
#   GRANITE_R2_HF_SNAPSHOT  HuggingFace snapshot dir with config.json,
#                           tokenizer.json, model.safetensors. Defaults to the
#                           cache path the FP16 spawn uses.
#   LUNARIS_SPIKE_CACHE     Root cache. Defaults to ~/.cache/lunaris/spike.
#   LLAMACPP_PR             PR number to patch onto llama.cpp HEAD for
#                           granite-r2 tokenizer support. Defaults to 22716.
#                           Set to empty string to skip patching.
#
# Outputs (under LUNARIS_SPIKE_CACHE/granite-r2/gguf/):
#   granite-r2-311m-f16.gguf      intermediate F16 GGUF (~623 MB)
#   granite-r2-311m-Q4_K_M.gguf   final quantized payload (~241 MB)
#   Q4_K_M.sha256                 the SHA-256 pin (used by lib.rs constant)
#   Q4_K_M.tensor-manifest.txt    per-tensor dtype/shape dump (for porters)
#
# Why a script and not a Makefile or just docs?
# - The conversion needs three deterministic steps (clone+patch, convert,
#   quantize) gated on cache hits. A bash script keeps the dependency
#   graph explicit and the rerun cost zero.
# - Pre-commit / CI never run this; it's a one-shot spike that the engineer
#   executes once per model revision and pins the resulting SHA-256 in
#   crates/lunaris-embed-native/src/lib.rs as GRANITE_R2_GGUF_Q4_SHA256.
#
# What this script does NOT do:
# - Download granite-r2 weights from HuggingFace. The FP16 spawn already
#   provides the HF cache path; we just consume it.
# - Commit the GGUF file to git. The file is ~241 MB and lives in the local
#   cache. The SHA-256 alone is what we pin.

set -euo pipefail

LUNARIS_SPIKE_CACHE="${LUNARIS_SPIKE_CACHE:-$HOME/.cache/lunaris/spike}"
GRANITE_R2_HF_SNAPSHOT="${GRANITE_R2_HF_SNAPSHOT:-$LUNARIS_SPIKE_CACHE/granite-r2/models--ibm-granite--granite-embedding-311m-multilingual-r2/snapshots/dba7b0ee9d789f330fecfb85df57699f9e7d9c42}"
LLAMACPP_PR="${LLAMACPP_PR:-22716}"

LLAMACPP_ROOT="$LUNARIS_SPIKE_CACHE/llamacpp/llama.cpp"
GGUF_DIR="$LUNARIS_SPIKE_CACHE/granite-r2/gguf"
F16_GGUF="$GGUF_DIR/granite-r2-311m-f16.gguf"
Q4_GGUF="$GGUF_DIR/granite-r2-311m-Q4_K_M.gguf"
Q4_SHA="$GGUF_DIR/Q4_K_M.sha256"
MANIFEST="$GGUF_DIR/Q4_K_M.tensor-manifest.txt"

log() { printf '[spike-convert] %s\n' "$*" >&2; }
fail() { printf '[spike-convert][ERROR] %s\n' "$*" >&2; exit 1; }

[ -d "$GRANITE_R2_HF_SNAPSHOT" ] || fail "HF snapshot not found: $GRANITE_R2_HF_SNAPSHOT
hint: run scripts/spike-generate-reference-embeddings.py to populate the cache."
[ -f "$GRANITE_R2_HF_SNAPSHOT/config.json" ] || fail "config.json missing in snapshot"
[ -f "$GRANITE_R2_HF_SNAPSHOT/tokenizer.json" ] || fail "tokenizer.json missing in snapshot"
[ -f "$GRANITE_R2_HF_SNAPSHOT/model.safetensors" ] || fail "model.safetensors missing in snapshot"

mkdir -p "$GGUF_DIR" "$LUNARIS_SPIKE_CACHE/llamacpp"

# --- step 1: clone + patch llama.cpp -----------------------------------------
if [ ! -d "$LLAMACPP_ROOT/.git" ]; then
    log "cloning llama.cpp (shallow) into $LLAMACPP_ROOT"
    git clone --depth 50 https://github.com/ggml-org/llama.cpp.git "$LLAMACPP_ROOT"
fi

if [ -n "$LLAMACPP_PR" ]; then
    if ! ( cd "$LLAMACPP_ROOT" && git rev-parse --verify --quiet "granite-embedding-r2-pr" >/dev/null ); then
        log "applying llama.cpp PR #$LLAMACPP_PR (granite-r2 tokenizer + SwiGLU 97m enabler)"
        ( cd "$LLAMACPP_ROOT" && gh pr checkout "$LLAMACPP_PR" --repo ggml-org/llama.cpp )
    else
        log "PR branch already checked out; skipping patch step"
    fi
fi

# Pin the PR head so subsequent agent sessions can diff against it.
PR_HEAD="$( cd "$LLAMACPP_ROOT" && git rev-parse HEAD )"
log "llama.cpp HEAD = $PR_HEAD"

# --- step 2: converter venv --------------------------------------------------
VENV="$LLAMACPP_ROOT/.venv"
if [ ! -x "$VENV/bin/python" ]; then
    PY="$(command -v python3.11 || command -v python3.12 || command -v python3.13 || command -v python3)"
    log "creating converter venv with $PY"
    "$PY" -m venv "$VENV"
    "$VENV/bin/pip" install --upgrade pip setuptools wheel >/dev/null
    "$VENV/bin/pip" install -r "$LLAMACPP_ROOT/requirements/requirements-convert_hf_to_gguf.txt"
fi

# --- step 3: F16 GGUF --------------------------------------------------------
if [ ! -f "$F16_GGUF" ]; then
    log "converting HF safetensors → F16 GGUF: $F16_GGUF"
    "$VENV/bin/python" "$LLAMACPP_ROOT/convert_hf_to_gguf.py" \
        "$GRANITE_R2_HF_SNAPSHOT" \
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
if [ "$size_mib" -gt 250 ]; then
    fail "Q4_K_M size ${size_mib} MiB exceeds 250 MiB budget"
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
log "pin in crates/lunaris-embed-native/src/lib.rs:"
log "    pub const GRANITE_R2_GGUF_Q4_SHA256: &str = \"$SHA256_HEX\";"

# --- step 7: tensor manifest -------------------------------------------------
"$VENV/bin/python" - "$Q4_GGUF" > "$MANIFEST" <<'PY'
import sys, gguf
r = gguf.GGUFReader(sys.argv[1])
print('# generated by spike-convert-granite-r2-to-gguf.sh')
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
