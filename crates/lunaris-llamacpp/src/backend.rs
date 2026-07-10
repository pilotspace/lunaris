//! Process-global `LlamaBackend` — llama.cpp's backend init
//! (`llama_backend_init`) is once-per-process, so every model in this crate
//! (embedder, reranker, later extractor) must share one instance. The
//! `OnceLock` closure runs exactly once; the backend is intentionally never
//! freed (process lifetime — same pattern as a global runtime).

use std::sync::OnceLock;

use llama_cpp_2::llama_backend::LlamaBackend;

/// Get the shared backend, initializing it on first use. Returns the init
/// error message verbatim on failure (subsequent calls re-observe the same
/// cached error — llama.cpp init failures are not transient).
pub(crate) fn shared_backend() -> Result<&'static LlamaBackend, String> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| LlamaBackend::init().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}
