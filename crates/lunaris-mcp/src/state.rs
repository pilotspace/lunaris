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
}

impl AppState {
    /// Build `AppState` from CLI arguments.
    ///
    /// Steps:
    /// 1. Resolve the active scope via [`crate::scope_resolver::resolve`].
    /// 2. Derive or use the caller-supplied storage URL.
    /// 3. Open the Lunaris engine (async; may run DB migrations).
    ///
    /// **Cold-start budget note:** `Lunaris::open` with `embedder-gguf` feature
    /// active but no GGUF staged silently falls back to `NoopEmbedder` — it does
    /// NOT trigger a model download. Do NOT call `model_stager::ensure_staged`
    /// here. That call is deferred to the `memory.recall` handler (Wave 2.B).
    pub(crate) async fn bootstrap(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
    ) -> Result<Self, BootstrapError> {
        let scope = crate::scope_resolver::resolve(scope_override)?;
        let storage_url = resolve_storage_url(storage_override, &scope)?;
        tracing::info!(
            scope   = scope.as_str(),
            storage = %storage_url,
            "opening lunaris engine",
        );
        let lunaris = Lunaris::open(&storage_url).await?;
        Ok(Self { lunaris: Arc::new(lunaris), scope })
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Derive the SQLite storage URL for the given scope.
///
/// Priority:
/// 1. Caller-supplied `override_` (passed through verbatim).
/// 2. `sqlite:///<HOME>/.lunaris/<scope>.db` — created if absent.
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
