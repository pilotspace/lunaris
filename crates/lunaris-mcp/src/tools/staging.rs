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

// ── Namespace validator ───────────────────────────────────────────────────────

/// Validate a caller-supplied namespace string.
///
/// Namespace shapes the `scope_prefix` passed to `WorkingMemory::new`.
/// It is NOT a security boundary (scope is — bound at startup), but it
/// MUST NOT contain `:` to prevent source-key byte-aliasing across namespaces
/// (same rationale as `Scope::new`'s v0.2.1 alphabet tightening).
///
/// Allowed: `[A-Za-z0-9_\-./]`, length 1..=128.
/// Default when `Option::None` is supplied by caller: `"scratchpad/"`.
// Used by scratchpad_write, scratchpad_read, scratchpad_grep (added in T2/T3).
#[allow(dead_code)]
pub(crate) fn validate_namespace(ns: &str) -> Result<(), ToolError> {
    if ns.is_empty() || ns.len() > 128 {
        return Err(ToolError::InvalidInput(
            "namespace must be 1..=128 chars of [A-Za-z0-9_\\-./]; ':' is not allowed to prevent source-key aliasing".into(),
        ));
    }
    for c in ns.chars() {
        if !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '.' | '/') {
            return Err(ToolError::InvalidInput(
                "namespace must be 1..=128 chars of [A-Za-z0-9_\\-./]; ':' is not allowed to prevent source-key aliasing".into(),
            ));
        }
    }
    Ok(())
}

/// Resolve the namespace from an optional wire value, validate it, and return it.
/// Returns "scratchpad/" if `None`. Returns `Err` if the supplied value fails validation.
// Used by scratchpad_consolidate (explicit-namespace tool semantics).
#[allow(dead_code)]
pub(crate) fn resolve_namespace(ns: Option<String>) -> Result<String, ToolError> {
    match ns {
        None => Ok("scratchpad/".to_owned()),
        Some(s) => {
            validate_namespace(&s)?;
            Ok(s)
        }
    }
}

/// Session-aware namespace resolution (scratchpad-handover task) — used by
/// scratchpad_write / scratchpad_read / scratchpad_grep.
///
/// An EXPLICIT namespace wins verbatim (validated, byte-identical to
/// [`resolve_namespace`] semantics — the session logic never touches it).
/// With `None`, the default follows the sessions.json marker that
/// lunaris-hook maintains: `scratchpad/{active_session_id}/` when a marker
/// names an active session for this scope, `scratchpad/` otherwise
/// (back-compat when the hook is not installed).
///
/// When the active session CHANGED since this process last served a pad
/// (or on the first call after a restart with a marker present), the
/// previous session's pending events are first consolidated via the guarded
/// whole-scope drain — warn-and-continue, never an error on this call.
pub(crate) async fn resolve_namespace_session_aware(
    state: &crate::state::AppState,
    ns: Option<String>,
) -> Result<String, ToolError> {
    if let Some(s) = ns {
        validate_namespace(&s)?;
        return Ok(s);
    }
    let path = crate::session_pad::sessions_file_path();
    let scope = state.scope.as_str();
    if crate::session_pad::take_pending_handover_at(&path, scope) {
        crate::tools::scratchpad_consolidate::run_handover_consolidate(state).await;
    }
    let active = crate::session_pad::active_session_at(&path, scope);
    Ok(crate::session_pad::default_namespace(active.as_deref()))
}

// The keyword-NotSupported classifier now lives in `lunaris_memory_service`
// (the recall handler moved there); the scratchpad handlers that remain here
// do not use it.

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_namespace ────────────────────────────────────────────────────

    #[test]
    fn valid_namespace_scratchpad_slash() {
        assert!(validate_namespace("scratchpad/").is_ok());
    }

    #[test]
    fn valid_namespace_single_char() {
        assert!(validate_namespace("a").is_ok());
    }

    #[test]
    fn valid_namespace_path_with_underscores_and_dots() {
        assert!(validate_namespace("path/with-underscores_and.dots/").is_ok());
    }

    #[test]
    fn valid_namespace_128_chars() {
        let s = "a".repeat(128);
        assert!(validate_namespace(&s).is_ok());
    }

    #[test]
    fn invalid_namespace_empty() {
        assert!(validate_namespace("").is_err());
    }

    #[test]
    fn invalid_namespace_129_chars() {
        let s = "a".repeat(129);
        let err = validate_namespace(&s).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("128") || msg.contains("1..=128"),
            "expected length mention; got: {msg}"
        );
    }

    #[test]
    fn invalid_namespace_colon() {
        let err = validate_namespace("a:b").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(':') || msg.contains("aliasing"), "expected ':' mention; got: {msg}");
    }

    #[test]
    fn invalid_namespace_space() {
        assert!(validate_namespace("has space").is_err());
    }

    #[test]
    fn invalid_namespace_at_sign() {
        assert!(validate_namespace("has@at").is_err());
    }
}
