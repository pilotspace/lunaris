//! Shared staging gate, namespace validator, and keyword-NotSupported helper
//! for all MCP tool handlers that require the embedder.
//!
//! Moved from `tools/recall.rs` so `scratchpad_read` and `scratchpad_grep`
//! can reuse the exact same staging path and keyword-fallback detector.
//! There is exactly ONE definition of each item here — recall.rs and the
//! scratchpad handlers import from this module.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::OnceCell;

use crate::model_stager::{ModelKind, StageError, ensure_staged};
use crate::tools::ToolError;

// ── Lazy stager ───────────────────────────────────────────────────────────────

/// Printed at most once per process on the first model download.
pub(crate) static STAGE_LOG_ONCE: OnceLock<()> = OnceLock::new();

/// Successful model staging verification is process-stable.
///
/// `ensure_staged` hashes the full GGUF when the file already exists. That is
/// the right integrity check before first use, but doing it on every recall
/// burns one CPU core for seconds on a 253 MB model. Cache only successful
/// verification; transient failures are retried on the next recall.
pub(crate) static STAGED_MODEL: OnceCell<()> = OnceCell::const_new();

/// Test seam: when `true`, `maybe_ensure_staged` skips the real GGUF download.
///
/// Set by `#[cfg(test)]` helpers via `skip_stage_for_tests()`. Production code
/// never sets this — the value starts `false` and only flips inside the test
/// binary. This avoids `unsafe { std::env::set_var }` under `forbid(unsafe_code)`.
pub(crate) static SKIP_STAGE: AtomicBool = AtomicBool::new(false);

/// Enable the staging bypass in the current process (test-only call site).
#[cfg(test)]
pub(crate) fn skip_stage_for_tests() {
    SKIP_STAGE.store(true, Ordering::Relaxed);
}

/// Ensure the embedder GGUF is staged — called lazily from the first recall or read.
///
/// Bypassed when:
/// - `LUNARIS_MCP_SKIP_STAGE` env var is set (CI / operator override), OR
/// - `SKIP_STAGE` atomic is `true` (test seam — no `unsafe` needed).
pub(crate) async fn maybe_ensure_staged() -> Result<(), ToolError> {
    if SKIP_STAGE.load(Ordering::Relaxed) || std::env::var_os("LUNARIS_MCP_SKIP_STAGE").is_some() {
        return Ok(());
    }
    STAGED_MODEL
        .get_or_try_init(|| async {
            STAGE_LOG_ONCE.get_or_init(|| {
                eprintln!("lunaris-mcp: staging models — first run only");
            });
            ensure_staged(ModelKind::EmbedderGraniteQ4KM).await.map(|_| ()).map_err(
                |e: StageError| ToolError::InvalidInput(format!("model staging failed: {e}")),
            )
        })
        .await
        .map(|_| ())
}

// ── Session-aware namespace resolution ─────────────────────────────────────────
//
// The namespace VALIDATOR + plain resolver now live in the shared crate
// (`lunaris_memory_service::namespace`) alongside the scratchpad handlers. Only
// the SESSION-aware resolution stays here: it reads the sessions.json marker,
// which is client-machine state maintained by lunaris-hook next to this mcp
// process (contextd has no marker), so it is inherently a caller concern.

/// Session-aware namespace resolution (scratchpad-proxiable task) — used by the
/// scratchpad_write / scratchpad_read / scratchpad_grep `#[tool]` methods.
///
/// An EXPLICIT namespace wins verbatim (validated via the shared validator; the
/// session logic never touches it). With `None`, the default follows the
/// sessions.json marker that lunaris-hook maintains: `scratchpad/{active_session_id}/`
/// when a marker names an active session for this scope, `scratchpad/` otherwise
/// (back-compat when the hook is not installed).
///
/// When the active session CHANGED since this process last served a pad (or on
/// the first call after a restart with a marker present), the previous session's
/// pending events are first consolidated by firing a `ScratchpadHandover`
/// THROUGH the proxy — so the whole-scope drain runs on the warm engine that
/// owns the pad (contextd over the socket, or the direct-open fallback), NOT a
/// second local engine. Warn-and-continue: a handover dispatch failure NEVER
/// errors this call (the marker flag is already cleared by
/// `take_pending_handover_at`, and the drain is retried at the next switch).
pub(crate) async fn resolve_namespace_session_aware(
    proxy: &crate::proxy::MemoryProxy,
    state: &crate::state::AppState,
    ns: Option<String>,
) -> Result<String, ToolError> {
    if let Some(s) = ns {
        lunaris_memory_service::namespace::validate(&s)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        return Ok(s);
    }
    let path = crate::session_pad::sessions_file_path();
    let scope = state.scope.as_str();
    if crate::session_pad::take_pending_handover_at(&path, scope) {
        let req = lunaris_memory_service::protocol::MemoryRequest::ScratchpadHandover {
            scope: scope.to_owned(),
        };
        if let Err(e) = proxy.dispatch(state, req).await {
            tracing::warn!(
                scope,
                err = ?e,
                "session handover dispatch failed — previous pad carries forward, \
                 retrying at the next switch",
            );
        }
    }
    let active = crate::session_pad::active_session_at(&path, scope);
    Ok(crate::session_pad::default_namespace(active.as_deref()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// The namespace VALIDATOR tests moved with the validator to the shared crate
// (`lunaris_memory_service::namespace`). What remains here is the mcp-side
// SESSION-aware resolution: explicit-namespace validation + the marker-driven
// per-session default (with the handover firing THROUGH the proxy).

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris_core::{Scope, StubEmbedder};
    use serde_json::json;

    use super::*;
    use crate::proxy::MemoryProxy;
    use crate::state::AppState;
    use lunaris_test_harness::{TestStore, open_test_engine_with_embedder};

    /// 0.7.0 port off `memory://` — harness-issued ephemeral Moon, degrading to
    /// `memory://` where no Moon binary resolves. `AppState` holds an
    /// `Arc<Lunaris>`, so `TestEngine` is split via `into_parts()`; the returned
    /// [`TestStore`] owns the Moon child and must stay bound for the test.
    async fn fresh_state(scope_name: &str) -> (AppState, TestStore) {
        skip_stage_for_tests();
        let embedder = Arc::new(StubEmbedder::new(768));
        let (lunaris, store) = open_test_engine_with_embedder(embedder).await.into_parts();
        let scope = Scope::new(scope_name).unwrap();
        let state = AppState {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None,
        };
        (state, store)
    }

    /// An EXPLICIT namespace is validated via the shared validator and returned
    /// verbatim — the session marker is never consulted. A `:` is rejected.
    #[tokio::test]
    async fn explicit_namespace_wins_and_colon_rejected() {
        let (state, _store) = fresh_state("test-staging-explicit").await;
        let proxy = MemoryProxy::direct_only_for_test();

        let ok = resolve_namespace_session_aware(&proxy, &state, Some("notes/".into()))
            .await
            .expect("valid explicit namespace passes through");
        assert_eq!(ok, "notes/");

        let bad = resolve_namespace_session_aware(&proxy, &state, Some("a:b".into())).await;
        assert!(
            matches!(bad, Err(ToolError::InvalidInput(_))),
            "explicit namespace with ':' must be rejected; got: {bad:?}"
        );
    }

    /// scratchpad-proxiable: with a sessions.json marker naming an active
    /// session, the DEFAULT namespace (ns=None) becomes the per-session pad.
    /// The handover fires THROUGH the proxy (Direct-only here) and must NOT
    /// error the resolution — on a backend without a native queue it guard-
    /// skips, on Moon it runs the real drain; either way the resolution stands.
    #[tokio::test]
    async fn default_namespace_follows_session_marker_via_proxy() {
        let _seam = crate::session_pad::lock_test_seam().await;

        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("sessions.json");
        let body = json!({
            "test-staging-session": { "active_session_id": "sess-77", "ended": false,
                                      "updated_at": "2026-06-11T00:00:00Z" }
        });
        std::fs::write(&marker, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        crate::session_pad::set_sessions_file_for_tests(Some(marker.clone()));

        let (state, _store) = fresh_state("test-staging-session").await;
        let proxy = MemoryProxy::direct_only_for_test();

        let ns = resolve_namespace_session_aware(&proxy, &state, None)
            .await
            .expect("session-aware default resolution must not error");
        assert_eq!(
            ns, "scratchpad/sess-77/",
            "default namespace with a marker present must be the per-session pad"
        );

        crate::session_pad::set_sessions_file_for_tests(None);
    }
}
