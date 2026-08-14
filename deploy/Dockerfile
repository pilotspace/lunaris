# syntax=docker/dockerfile:1.7
# =============================================================================
# lunaris-server — multi-stage image
# =============================================================================
#
# Build from the REPOSITORY ROOT (the workspace + the `vendor/moon` submodule
# must both be inside the build context):
#
#   git submodule update --init vendor/moon
#   docker build -f deploy/Dockerfile -t lunaris-server:dev .
#
# or, via compose (which already sets `context: ..`):
#
#   docker compose -f deploy/docker-compose.yml build
#
# ── Why the submodule is mandatory ───────────────────────────────────────────
# `Cargo.toml:158` declares the Moon SDK as a PATH dependency:
#   moon = { path = "vendor/moon/sdk/rust", version = "0.2.1", package = "moondb" }
# A path dependency always wins over the crates.io release inside a workspace,
# so `cargo build` fails outright ("failed to read vendor/moon/sdk/rust/
# Cargo.toml") when the submodule is not checked out. The builder asserts this
# up front rather than letting cargo fail 200 lines into the log.
#
# ── Why `--features lunaris/llamacpp` ────────────────────────────────────────
# `Cargo.toml:101` pins the workspace dep as
#   lunaris = { path = "crates/lunaris", default-features = false, package = "lunaris-memory" }
# and `crates/lunaris-server/Cargo.toml:19` takes it with `workspace = true`.
# A bare `cargo build -p lunaris-server` therefore produces a **Tier-0**
# binary: `lunaris-llamacpp` is not in the graph at all, `resolve_embedder()`
# falls through to `NoopEmbedder`, and every vector recall returns zero rows.
# That is a valid small-device build; it is NOT a production server. Verified:
#   cargo tree -p lunaris-server --features lunaris/llamacpp -i lunaris-llamacpp
#     -> lunaris-llamacpp -> lunaris-memory -> lunaris-server
#   cargo tree -p lunaris-server -i lunaris-llamacpp
#     -> "package ID specification `lunaris-llamacpp` did not match any packages"
#
# `llamacpp` compiles llama.cpp, which needs cmake + a C++ toolchain +
# libclang (bindgen, via llama-cpp-sys-2) — the same three the release
# workflows install (.github/workflows/python-prebuild.yml:126-138,
# ts-prebuild.yml:116-128).
#
# ── GPU ──────────────────────────────────────────────────────────────────────
# Device support is a BUILD-time feature (`metal` / `cuda` / `vulkan` in
# crates/lunaris/Cargo.toml). This image is the portable CPU build. For CUDA,
# swap the base for an nvidia/cuda devel image and set
# FEATURES=lunaris/cuda — untested here, do not assume it just works.
# =============================================================================

ARG RUST_VERSION=1.94
ARG DEBIAN_VERSION=bookworm

# -----------------------------------------------------------------------------
# Stage 1: builder
# -----------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS builder

# cmake           — llama-cpp-sys-2 (and aws-lc-sys) drive their C/C++ builds through it
# build-essential — the C++ compiler llama.cpp needs
# libclang-dev    — bindgen 0.72 inside llama-cpp-sys-2 ("Unable to find libclang")
# pkg-config      — several -sys crates
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        build-essential \
        libclang-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
ENV CARGO_INCREMENTAL=0

WORKDIR /src
COPY . .

# Fail with an operator-legible message instead of a cargo backtrace.
RUN test -f vendor/moon/sdk/rust/Cargo.toml || { \
        echo "ERROR: vendor/moon is not checked out."; \
        echo "       Run 'git submodule update --init vendor/moon' and rebuild."; \
        exit 1; \
    }

# `lunaris/llamacpp` = the `llamacpp` feature of the dependency this crate
# names `lunaris` (real package: lunaris-memory). See the header note.
ARG FEATURES=lunaris/llamacpp
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p lunaris-server --features ${FEATURES} \
    && cp /src/target/release/lunaris-server /usr/local/bin/lunaris-server

# -----------------------------------------------------------------------------
# Stage 2: runtime
# -----------------------------------------------------------------------------
# debian-slim, NOT distroless: llama.cpp links libstdc++ and libgomp (OpenMP),
# and the compose healthcheck needs a HTTP client in the image. Distroless
# would buy ~60MB at the cost of an unprobeable container.
FROM debian:${DEBIAN_VERSION}-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libgomp1 \
        libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --create-home --home-dir /home/lunaris lunaris

LABEL org.opencontainers.image.title="lunaris-server" \
      org.opencontainers.image.description="Lunaris agent memory engine — MemoryProtocol HTTP+SSE server" \
      org.opencontainers.image.source="https://github.com/pilotspace/lunaris" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /usr/local/bin/lunaris-server /usr/local/bin/lunaris-server

# GGUF weights are NOT baked in (multi-GB, separately licensed). Mount them
# and point LUNARIS_EMBEDDER_GGUF / LUNARIS_RERANKER_GGUF at the mount —
# see docs/operations/external-moon.md §7.
RUN mkdir -p /models /etc/lunaris && chown lunaris:lunaris /models /etc/lunaris
VOLUME ["/models"]

USER lunaris
EXPOSE 8080

# Kubernetes semantics (crates/lunaris-server/src/routes/readyz.rs:9-14):
#   livenessProbe  -> /healthz   readinessProbe -> /readyz
HEALTHCHECK --interval=15s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/readyz || exit 1

ENTRYPOINT ["/usr/local/bin/lunaris-server"]
