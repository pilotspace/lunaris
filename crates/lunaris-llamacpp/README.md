# lunaris-llamacpp

In-process [llama.cpp](https://github.com/ggml-org/llama.cpp) inference
runtime for [Lunaris](https://github.com/pilotspace/lunaris) — the sole
local backend for the embedder and reranker since the v0.6 llama.cpp-only
cutover.

- **Embedder** — GGUF embedding models (default:
  `ibm-granite/granite-embedding-311m-multilingual-r2`, Q4_K_M).
- **Reranker** — GGUF cross-encoder models (default:
  `BAAI/bge-reranker-v2-m3`, Q5_K_M), lazy-loaded on first recall.
- Feature-gated behind `llamacpp` (needs cmake + a C++ toolchain);
  GPU offload via the build-time `metal` / `cuda` / `vulkan` features.
- `shutdown_all_inference()` tears engines down before process exit so
  ggml backends (e.g. Metal residency sets) release cleanly.

This crate is an internal building block of the `lunaris-memory` umbrella
crate; depend on the umbrella unless you are embedding the runtime directly.
