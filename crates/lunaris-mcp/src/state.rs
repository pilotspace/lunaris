//! Shared application state for the Lunaris MCP server.
//!
//! `AppState` holds an `Arc<Lunaris>` handle and the resolved `Scope` for
//! this server process. The scope is bound at startup; wire payloads cannot
//! override it (CLAUDE.md §JWT/scope discipline applies here too — the stdio
//! transport is process-bound, so the scope comes from the CLI/env, never
//! from client-supplied tool arguments).
//!
//! `ScopedLunaris<'_>` borrows from `&Lunaris` and must NOT be stored in
//! state. Re-derive it per tool call: `state.lunaris.scoped(state.scope.clone())`.

use std::{io, sync::Arc};

use lunaris::Lunaris;
use lunaris_core::{Embedder, LunarisError, Scope};
use thiserror::Error;

// ── Error ────────────────────────────────────────────────────────────────────

/// Errors that can occur during MCP server bootstrap.
///
/// Returned by [`AppState::bootstrap`] — callers map this to an
/// `anyhow::Error` in `main.rs` so the process exits with a diagnostic.
#[derive(Debug, Error)]
pub(crate) enum BootstrapError {
    /// Scope resolution failed (bad override, no $HOME, or I/O on scopes.json).
    #[error("scope resolution: {0}")]
    Scope(#[from] crate::scope_resolver::ScopeResolveError),

    /// `Lunaris::open` returned an error (bad URL, DB migration failure, etc.).
    #[error("lunaris open: {0}")]
    LunarisOpen(#[from] LunarisError),

    /// Filesystem I/O failure while deriving the default storage path.
    #[error("storage path i/o: {0}")]
    Io(#[from] io::Error),

    /// Embedder resolved to `NoopEmbedder` (zero-vector fallback).
    ///
    /// Granite-r2 weights are not staged at the expected path, and no
    /// `LUNARIS_EMBEDDER_GGUF` env var was set. `vector_search` will always
    /// return empty hits when the embedder emits zero vectors.
    ///
    /// Fix: set `LUNARIS_EMBEDDER_GGUF=<path>` to an existing
    /// `granite-embedding-311m-multilingual-r2-Q4_K_M.gguf` file, or install
    /// FP16 weights via:
    ///   huggingface-cli download ibm-granite/granite-embedding-311m-multilingual-r2 \
    ///     --local-dir ~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/
    #[error(
        "embedder resolved to NoopEmbedder (zero-vector fallback) — vector recall will always \
         return empty hits.\n\
         Fix option A (GGUF, recommended for lunaris-mcp): set env var\n\
           LUNARIS_EMBEDDER_GGUF=<path/to/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf>\n\
         Fix option B (FP16 safetensors): install weights via\n\
           huggingface-cli download ibm-granite/granite-embedding-311m-multilingual-r2 \
         --local-dir ~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2/"
    )]
    NoEmbedderWeights,

    /// Embedder weights appear to be present but inference returned an error.
    ///
    /// The embedder is not `NoopEmbedder`, but calling `embed_batch` on a probe
    /// input failed. This usually means the GGUF or safetensors file is corrupt,
    /// the wrong architecture was selected, or the model requires more memory than
    /// is available.
    #[error("embedder health probe failed: {0}")]
    EmbedderUnhealthy(String),
}

// ── State ────────────────────────────────────────────────────────────────────

/// Env var that bypasses the embedder health probe in `AppState::bootstrap`.
///
/// When set to any non-empty value, [`probe_embedder_health`] is skipped and
/// the server starts even if the embedder resolves to [`NoopEmbedder`]. This
/// is an **operator / test escape hatch only** — recall returns empty hits
/// when the embedder is a noop. Production deployments must NOT set this.
///
/// Use cases:
/// - The `cold_start_under_500ms_no_gguf_staged` integration test sets this to
///   measure the server startup + `tools/list` latency without loading real
///   model weights (which would easily exceed the 500 ms budget even on fast
///   hardware).
/// - Automated benchmark harnesses that only call `memory.ingest` / metadata
///   APIs and never exercise `memory.recall`.
pub(crate) const SKIP_EMBEDDER_PROBE_ENV_VAR: &str = "LUNARIS_MCP_SKIP_EMBEDDER_PROBE";

/// Shared, cheaply-cloneable application state injected into every tool handler.
///
/// `lunaris` is `Arc`-wrapped so cloning `AppState` does not clone the engine.
/// `scope` is a validated newtype; cloning it per tool call is intentional and
/// cheap (it is a `String` behind an `Arc` in `Scope`'s impl).
///
/// **Do not store `ScopedLunaris<'_>` in this struct** — its lifetime borrows
/// from `&'_ Lunaris`, which cannot outlive the stack frame of a tool handler.
#[derive(Debug, Clone)]
pub(crate) struct AppState {
    /// High-level Lunaris engine handle (embedder + clock + storage wired in).
    pub(crate) lunaris: Arc<Lunaris>,
    /// Scope bound at server startup — all memory operations partition by this.
    pub(crate) scope: Scope,
    /// Owned embedded Moon guard — keeps the in-process Moon alive for the
    /// server's lifetime. `None` when the `embedded-moon` feature is OFF, when
    /// a `--storage` override was supplied, or when Moon bring-up failed and the
    /// circuit-breaker fell back to SQLite. `Arc<>` restores `Clone` on
    /// `AppState` (the inner `Mutex<Option<JoinHandle>>` is not `Clone`).
    #[cfg(feature = "embedded-moon")]
    pub(crate) _embedded_moon: Option<Arc<crate::embedded_moon::EmbeddedMoonGuard>>,
}

impl AppState {
    /// Build `AppState` from CLI arguments. Delegates to [`bootstrap_inner`] with
    /// `skip_probe = false`.
    ///
    /// Steps:
    /// 1. Resolve the active scope via [`crate::scope_resolver::resolve`].
    /// 2. Derive or use the caller-supplied storage URL.
    /// 3. Open the Lunaris engine (async; may run DB migrations).
    /// 4. Probe the embedder: if it emits zero vectors (`NoopEmbedder` fallback),
    ///    return [`BootstrapError::NoEmbedderWeights`] with an actionable hint.
    ///    If the embedder is present but inference fails, return
    ///    [`BootstrapError::EmbedderUnhealthy`].
    ///
    /// Step 4 is skipped when `LUNARIS_MCP_SKIP_EMBEDDER_PROBE` is set (see
    /// [`SKIP_EMBEDDER_PROBE_ENV_VAR`]). Operators may set this for benchmark /
    /// ingest-only workloads that never call `memory.recall`.
    ///
    /// **Cold-start note:** `Lunaris::open` with the `llamacpp` feature active
    /// but no GGUF staged silently falls back to `NoopEmbedder`. Step 4 catches
    /// this and converts the silent fallback into a loud, actionable error. Do
    /// NOT call `model_stager::ensure_staged` here — that call is deferred to
    /// the `memory.recall` handler (Wave 2.B) and only downloads on first recall,
    /// not at server start.
    pub(crate) async fn bootstrap(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
    ) -> Result<Self, BootstrapError> {
        Self::bootstrap_inner(scope_override, storage_override, false, None).await
    }

    /// Internal bootstrap with an explicit `skip_probe` flag.
    ///
    /// Step 4 (embedder health probe) is skipped when `skip_probe` is `true` OR
    /// when `LUNARIS_MCP_SKIP_EMBEDDER_PROBE` is set. The combined condition is:
    /// `skip_probe || env.is_some()`.
    ///
    /// `skip_probe` exists for tests that drive the REAL resolution path (Moon
    /// launch, guard wiring) without requiring real model weights. The operator env
    /// var continues to work for benchmark harnesses. Do NOT set `skip_probe = true`
    /// in production code.
    pub(crate) async fn bootstrap_inner(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
        skip_probe: bool,
        // Used only by the #[cfg(feature = "embedded-moon")] block below.
        // Suppressed on non-embedded-moon builds where only the cfg(not) branch runs.
        #[cfg_attr(not(feature = "embedded-moon"), allow(unused_variables))]
        data_dir_override: Option<&str>,
    ) -> Result<Self, BootstrapError> {
        let scope = crate::scope_resolver::resolve(scope_override)?;

        // Derive storage URL — and optionally launch embedded Moon.
        // When the embedded-moon feature is ON and no --storage override is
        // given, decide_storage_with_launcher starts run_embedded in-process
        // and returns the moon://127.0.0.1:<port> URL. On launch failure the
        // circuit-breaker emits tracing::warn and falls back to SQLite.
        #[cfg(feature = "embedded-moon")]
        let (storage_url, embedded_guard) = {
            let data_dir = data_dir_override.unwrap_or("./.lunaris-moon").to_owned();
            crate::embedded_moon::decide_storage_with_launcher(
                storage_override,
                &scope,
                move || {
                    // Clone into the async block so the future owns `data_dir`
                    // and does not borrow from the closure environment.
                    let dir = data_dir.clone();
                    async move { crate::embedded_moon::launch_embedded_moon(&dir).await }
                },
            )
            .await
        };
        // Map Option<EmbeddedMoonGuard> → Option<Arc<EmbeddedMoonGuard>>.
        #[cfg(feature = "embedded-moon")]
        let embedded_guard = embedded_guard.map(Arc::new);

        // When embedded-moon is OFF, use the existing sync helper unchanged.
        #[cfg(not(feature = "embedded-moon"))]
        let storage_url = resolve_storage_url(storage_override, &scope)?;

        tracing::info!(
            scope   = scope.as_str(),
            storage = %storage_url,
            "opening lunaris engine",
        );
        // MCP is a long-lived interactive daemon (like contextd): open with the
        // shared small-budget "interactive" embedder so its llama.cpp context
        // does not reserve the ~2.3 GB worst-case batch buffer. The embedder only
        // sees ~500-token chunks + short queries, so the 1024 budget never
        // truncates (LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS tunes it).
        let embedder = lunaris::resolve_default_embedder().await?;
        let lunaris = Lunaris::open_with_embedder(&storage_url, embedder).await?;

        // P-C (260609-dvi): install ActR consolidator; pipeline stays DISABLED so no
        // background worker is spawned — memory.scratchpad_consolidate is the SOLE
        // consumer. Force-installs regardless of LUNARIS_CONSOLIDATOR_BACKEND env var
        // so the MCP binary always has a real scoring consolidator available on demand.
        lunaris
            .consolidator_pipeline()
            .set_consolidator(
                Arc::new(lunaris::ActRConsolidator::default()) as Arc<dyn lunaris::Consolidator>
            );

        // Step 4: guard against the silent NoopEmbedder fallback.
        // Bypassed when skip_probe is true OR when LUNARIS_MCP_SKIP_EMBEDDER_PROBE
        // is set — so that latency benchmarks, test harnesses that only use ingest,
        // and discriminating bootstrap tests can start without real model weights.
        let skip = skip_probe || std::env::var_os(SKIP_EMBEDDER_PROBE_ENV_VAR).is_some();
        if !skip {
            probe_embedder_health(&lunaris.embedder()).await?;
        } else {
            tracing::warn!(
                env = SKIP_EMBEDDER_PROBE_ENV_VAR,
                skip_probe,
                "embedder health probe skipped — memory.recall may return empty hits \
                 if the embedder is NoopEmbedder"
            );
        }

        Ok(Self {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: embedded_guard,
        })
    }
}

/// Probe the embedder to detect the `NoopEmbedder` silent fallback.
///
/// Calls `embed_batch` on a short probe string and inspects the output:
///
/// - **All zeros** → the handle resolved to `NoopEmbedder`. Returns
///   [`BootstrapError::NoEmbedderWeights`] with an actionable install hint.
/// - **Non-zero vector** → real embedder is wired and healthy. Returns `Ok(())`.
/// - **`embed_batch` returns `Err`** → embedder is present but inference failed.
///   Returns [`BootstrapError::EmbedderUnhealthy`].
///
/// This is deliberately a `pub(crate)` free function so it can be unit-tested
/// by constructing `Arc<NoopEmbedder>` / `Arc<StubEmbedder>` directly, without
/// needing to call `AppState::bootstrap` or mutate environment variables (which
/// would require `unsafe` under `#![forbid(unsafe_code)]`).
pub(crate) async fn probe_embedder_health(
    embedder: &Arc<dyn Embedder>,
) -> Result<(), BootstrapError> {
    match embedder.embed_batch(&["lunaris embedder health probe"]).await {
        Err(e) => Err(BootstrapError::EmbedderUnhealthy(e.to_string())),
        Ok(vecs) => {
            let probe = vecs.into_iter().next().unwrap_or_default();
            let is_noop = probe.is_empty() || probe.iter().all(|&v| v == 0.0_f32);
            if is_noop { Err(BootstrapError::NoEmbedderWeights) } else { Ok(()) }
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Derive the SQLite storage URL for the given scope.
///
/// Priority:
/// 1. Caller-supplied `override_` (passed through verbatim).
/// 2. `sqlite:///<HOME>/.lunaris/<scope>.db` — created if absent.
///
/// Used only when the `embedded-moon` feature is OFF. When the feature is ON,
/// `decide_storage_with_launcher` handles both the Moon and SQLite fallback paths.
#[cfg(not(feature = "embedded-moon"))]
fn resolve_storage_url(override_: Option<&str>, scope: &Scope) -> Result<String, BootstrapError> {
    if let Some(url) = override_ {
        return Ok(url.to_owned());
    }

    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no $HOME directory found"))?;
    let dir = home.join(".lunaris");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.db", scope.as_str()));
    Ok(format!("sqlite://{}", path.display()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris_core::{NoopEmbedder, StubEmbedder};

    use super::*;

    /// RED test (root cause guard): `probe_embedder_health` must reject
    /// `NoopEmbedder` (zero vectors).
    ///
    /// Before the fix, `AppState::bootstrap` silently accepted the NoopEmbedder
    /// fallback from `resolve_embedder()` when model weights were missing. This
    /// test proves the fix is in place: a `NoopEmbedder`-backed probe returns
    /// `Err(BootstrapError::NoEmbedderWeights)`.
    ///
    /// Root cause: `mcp-recall-empty-hits` — `memory.recall` returned
    /// `{"hits":[]}` for every query because `NoopEmbedder::embed_batch` returns
    /// zero vectors, and `vector_search` short-circuits on `probe_norm == 0.0`.
    #[tokio::test]
    async fn probe_rejects_noop_embedder() {
        let noop: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
        let result = probe_embedder_health(&noop).await;
        assert!(
            matches!(result, Err(BootstrapError::NoEmbedderWeights)),
            "probe_embedder_health must return NoEmbedderWeights for NoopEmbedder; got: {result:?}"
        );
    }

    /// GREEN path: `probe_embedder_health` must accept `StubEmbedder` (non-zero
    /// deterministic vectors).
    #[tokio::test]
    async fn probe_accepts_stub_embedder() {
        let stub: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
        let result = probe_embedder_health(&stub).await;
        assert!(
            result.is_ok(),
            "probe_embedder_health must return Ok for StubEmbedder; got: {result:?}"
        );
    }

    /// Zero-dim noop embedder (empty vector output) is also rejected.
    #[tokio::test]
    async fn probe_rejects_zero_dim_noop() {
        let noop: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(0));
        let result = probe_embedder_health(&noop).await;
        assert!(
            matches!(result, Err(BootstrapError::NoEmbedderWeights)),
            "probe_embedder_health must reject dim=0 noop; got: {result:?}"
        );
    }

    // ── Discriminating bootstrap test (T1) ───────────────────────────────────

    /// Discriminating bootstrap test: driving the REAL production resolution path
    /// (AppState::bootstrap_inner) with --features embedded-moon MUST store an
    /// EmbeddedMoonGuard in _embedded_moon — proving the #[cfg(feature)] branch
    /// launched Moon, not the SQLite circuit-breaker fallback.
    ///
    /// If the cfg dispatch is ever broken (guard dropped early, branch wrong, guard
    /// not stored), _embedded_moon is None and this assertion fires — catching the
    /// built≠wired failure mode before npx/uvx distribution flips embedded-moon on.
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn bootstrap_launches_moon_not_sqlite_fallback() {
        use crate::tools::scratchpad_read::{ScratchpadReadParams, handle as read_handle};
        use crate::tools::scratchpad_write::{ScratchpadWriteParams, handle as write_handle};

        // best-effort cleanup: remove any stale .lunaris-moon from a previous run
        let _ = std::fs::remove_dir_all("./.lunaris-moon");

        let state = AppState::bootstrap_inner(None, None, true, None)
            .await
            .expect("bootstrap_inner(skip_probe=true) must succeed with --features embedded-moon");

        // THE discriminating assertion: guard must be stored (Moon was launched, not SQLite fallback)
        assert!(
            state._embedded_moon.is_some(),
            "feature=embedded-moon + no --storage override MUST launch embedded Moon and store \
             the guard; _embedded_moon is None, meaning the SQLite circuit-breaker silently took over"
        );

        // Round-trip through the production-constructed state to prove the path is live
        let write_resp = write_handle(
            &state,
            ScratchpadWriteParams {
                key: "bootstrap-wired".into(),
                value: serde_json::json!("ok"),
                namespace: None,
            },
        )
        .await
        .expect("scratchpad_write on bootstrap-produced state must succeed");
        assert!(!write_resp.lsn.is_empty(), "write response lsn must be non-empty");

        let read_resp = read_handle(
            &state,
            ScratchpadReadParams { key: "bootstrap-wired".into(), namespace: None },
        )
        .await
        .expect("scratchpad_read on bootstrap-produced state must succeed");
        assert!(read_resp.found, "key written via bootstrap state must be found on read");
        assert_eq!(
            read_resp.value,
            Some(serde_json::json!("ok")),
            "read value must match written value"
        );

        // cleanup
        drop(state);
        let _ = std::fs::remove_dir_all("./.lunaris-moon");
    }

    // ── GREEN tests for embedded-moon DI seam (T2) ────────────────────────────

    /// GREEN: decide_storage_with_launcher happy path + storage URL assertion.
    ///
    /// Calls the DI seam with the real launcher, asserts moon:// URL and that
    /// the handle cell is None after shutdown (task was taken+awaited).
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn decide_storage_real_launcher_wires_moon_url() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-wiring").unwrap();
        let tmpdir = tempfile::tempdir().unwrap();
        let data_dir = tmpdir.path().to_str().unwrap().to_owned();
        let (url, guard) =
            crate::embedded_moon::decide_storage_with_launcher(None, &scope, move || {
                let dir = data_dir.clone();
                async move { crate::embedded_moon::launch_embedded_moon(&dir).await }
            })
            .await;
        assert!(
            url.starts_with("moon://"),
            "decide_storage with real launcher must return moon:// URL, got: {url}"
        );
        assert!(guard.is_some(), "real launcher must produce a guard");
        if let Some(g) = guard {
            g.shutdown().await;
            assert!(g.handle.lock().is_none(), "handle cell must be None after shutdown");
        }
    }

    /// GREEN: --storage override bypasses embedded Moon entirely.
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn decide_storage_override_skips_embedded_moon() {
        use lunaris_core::Scope;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let scope = Scope::new("test-vuz-optout-state").unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        let (url, guard) = crate::embedded_moon::decide_storage_with_launcher(
            Some("sqlite:///override.db"),
            &scope,
            move || {
                c.store(true, Ordering::Relaxed);
                async move {
                    Err(crate::embedded_moon::EmbeddedMoonError::Timeout {
                        port: 0,
                        data_dir: "unused".into(),
                    })
                }
            },
        )
        .await;
        assert_eq!(url, "sqlite:///override.db");
        assert!(guard.is_none(), "--storage override must produce no guard");
        assert!(!called.load(Ordering::Relaxed), "launcher must NOT be called");
    }
}
