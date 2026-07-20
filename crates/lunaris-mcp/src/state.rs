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
use lunaris_core::{LunarisError, Scope};
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
}

// ── State ────────────────────────────────────────────────────────────────────

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
    /// Build `AppState` from CLI arguments.
    ///
    /// Steps:
    /// 1. Resolve the active scope via [`crate::scope_resolver::resolve`].
    /// 2. Derive or use the caller-supplied storage URL.
    /// 3. Open the Lunaris engine (async; may run DB migrations) with the
    ///    **lazy** default embedder — NO weights load at boot.
    ///
    /// Unified-inference contract (2026-07-19): embed-needing ops are normally
    /// served by the warm `lunaris-contextd` daemon over its socket, so this
    /// process must not park a second resident copy of the GGUF weights. The
    /// engine's embedder is [`lunaris::lazy_default_embedder`]: the resolve
    /// chain (GGUF → remote → Noop) only runs on the first in-process
    /// `embed_batch` — i.e. only when an embed op is genuinely served Direct
    /// (standalone installs, contextd unreachable). A Noop resolution surfaces
    /// as an actionable error on that first call, not as silent empty hits —
    /// which is why the old boot-time health probe (and its
    /// `LUNARIS_MCP_SKIP_EMBEDDER_PROBE` escape hatch) no longer exists.
    pub(crate) async fn bootstrap(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
    ) -> Result<Self, BootstrapError> {
        Self::bootstrap_inner(scope_override, storage_override, None).await
    }

    /// Internal bootstrap with a test-only data-dir override for embedded-moon.
    pub(crate) async fn bootstrap_inner(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
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
        // Lazy embedder: no weights load here. If a load ever happens (first
        // Direct embed op), it uses the shared small-budget "interactive"
        // resolve chain, so the llama.cpp context does not reserve the
        // ~2.3 GB worst-case batch buffer (LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS
        // tunes it).
        let embedder = lunaris::lazy_default_embedder();
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

        Ok(Self {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: embedded_guard,
        })
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
    // Only the embedded-moon tests below use items from the parent module.
    #[cfg(feature = "embedded-moon")]
    use super::*;

    // The old boot-time `probe_embedder_health` unit tests lived here. The
    // probe moved into `lunaris::lazy_default_embedder`'s first-load path
    // (unified-inference, 2026-07-19); its honest-error behavior is now pinned
    // end-to-end by `tests/lazy_embedder_boot.rs` against the real binary.

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
        use lunaris_memory_service::scratchpad_read::{
            ScratchpadReadParams, handle as read_handle,
        };
        use lunaris_memory_service::scratchpad_write::{
            ScratchpadWriteParams, handle as write_handle,
        };

        // best-effort cleanup: remove any stale .lunaris-moon from a previous run
        let _ = std::fs::remove_dir_all("./.lunaris-moon");

        let state = AppState::bootstrap_inner(None, None, None)
            .await
            .expect("bootstrap_inner must succeed with --features embedded-moon");

        // THE discriminating assertion: guard must be stored (Moon was launched, not SQLite fallback)
        assert!(
            state._embedded_moon.is_some(),
            "feature=embedded-moon + no --storage override MUST launch embedded Moon and store \
             the guard; _embedded_moon is None, meaning the SQLite circuit-breaker silently took over"
        );

        // Round-trip through the production-constructed state to prove the path is live
        let write_resp = write_handle(
            &state.lunaris,
            &state.scope,
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
            &state.lunaris,
            &state.scope,
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
