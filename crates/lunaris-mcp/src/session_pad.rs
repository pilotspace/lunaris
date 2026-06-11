//! Per-session scratchpad resolution + handover tracking (scratchpad-handover task).
//!
//! Read-side mirror of the `~/.lunaris/sessions.json` marker that
//! `lunaris-hook` maintains (session-switch-detect task). Same dual-impl
//! pattern as the two `JsonScopesFileStore`s: each binary owns its struct,
//! neither imports the other, both agree on the on-disk format.
//!
//! # The convention
//!
//! When the marker names an active session for the server's scope, the
//! DEFAULT scratchpad namespace becomes `scratchpad/{sanitized_session_id}/`.
//! With no marker (hook not installed) the default stays `scratchpad/`
//! (back-compat). An EXPLICIT `namespace` param always wins, verbatim.
//!
//! # Handover
//!
//! On the first scratchpad tool call after the active session changes (or
//! after a server restart with a marker present), the previous session's
//! pending consolidate events are drained via the guarded WHOLE-SCOPE
//! consolidate — NOT prefix-filtered: `consolidate_scoped(Some(prefix))`
//! drops drained non-matching events (see task consolidate-prefix-drop), so
//! a prefix-filtered handover would silently eat other namespaces' events.
//! Failures and guard refusals warn and carry forward; they NEVER error the
//! tool call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Marker file shape — mirrors lunaris-hook's `session_marker::MarkerEntry`.
/// Lenient: only `active_session_id` is required so a hook-side field
/// addition never breaks the MCP reader.
#[derive(Debug, Clone, serde::Deserialize)]
struct MarkerEntry {
    active_session_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    ended: bool,
}

/// Test seam: overrides the sessions-file location for in-process tests
/// (mirrors the `SKIP_STAGE` atomic pattern — no `unsafe env::set_var`).
static SESSIONS_FILE_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Last session id this PROCESS served per scope. The disk marker is the
/// source of truth for "active"; this map only answers "did it change since
/// we last looked" — empty after a restart, which deliberately fires one
/// handover (restart-safe; the drain is a no-op when the queue is empty).
static LAST_SERVED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn set_sessions_file_for_tests(p: Option<PathBuf>) {
    *SESSIONS_FILE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("sessions override lock") = p;
}

/// Serializes tests that use the process-global sessions-file seam.
/// tokio Mutex (not std/parking_lot) so the guard may live across `.await`
/// without violating the lock-across-await discipline.
#[cfg(test)]
static TEST_SEAM_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub(crate) async fn lock_test_seam() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_SEAM_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
}

/// Resolve the sessions file path: test-seam override, then
/// `LUNARIS_SESSIONS_FILE`, then `~/.lunaris/sessions.json`.
pub(crate) fn sessions_file_path() -> PathBuf {
    if let Some(lock) = SESSIONS_FILE_OVERRIDE.get()
        && let Some(p) = lock.lock().expect("sessions override lock").clone()
    {
        return p;
    }
    if let Some(p) = std::env::var_os("LUNARIS_SESSIONS_FILE") {
        return PathBuf::from(p);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".lunaris").join("sessions.json")
}

/// Read the active (sanitized) session id for `scope` from the marker at
/// `path`. `None` on missing file / missing entry / any IO or parse error
/// (same tolerance the hook side has).
pub(crate) fn active_session_at(path: &Path, scope: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let map: HashMap<String, MarkerEntry> = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(path = %path.display(), err = %e, "sessions marker corrupt — ignoring");
            return None;
        }
    };
    map.get(scope).map(|e| e.active_session_id.clone())
}

/// The default scratchpad namespace for an optional active session:
/// `scratchpad/{id}/` when present, `scratchpad/` otherwise.
pub(crate) fn default_namespace(active: Option<&str>) -> String {
    match active {
        Some(id) => format!("scratchpad/{id}/"),
        None => "scratchpad/".to_owned(),
    }
}

/// Returns `true` exactly once per (process, scope, active-session) change:
/// the first call after the marker's active session differs from what this
/// process last served (or after a restart with a marker present). The
/// caller runs the guarded whole-scope consolidate when this fires.
pub(crate) fn take_pending_handover_at(path: &Path, scope: &str) -> bool {
    let Some(active) = active_session_at(path, scope) else {
        // No marker (hook not installed) — never trigger handover work.
        return false;
    };
    let mut map =
        LAST_SERVED.get_or_init(|| Mutex::new(HashMap::new())).lock().expect("last-served lock");
    match map.get(scope) {
        Some(last) if *last == active => false,
        _ => {
            map.insert(scope.to_owned(), active);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_marker(dir: &tempfile::TempDir, scope: &str, session: &str) -> PathBuf {
        let path = dir.path().join("sessions.json");
        let body = serde_json::json!({
            scope: { "active_session_id": session, "ended": false,
                     "updated_at": "2026-06-11T00:00:00Z" }
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        path
    }

    #[test]
    fn active_session_reads_task1_marker_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_marker(&tmp, "scope-a", "sess-1");
        assert_eq!(active_session_at(&path, "scope-a"), Some("sess-1".to_owned()));
        assert_eq!(active_session_at(&path, "scope-other"), None, "scopes are independent");
    }

    #[test]
    fn corrupt_or_missing_marker_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        assert_eq!(active_session_at(&missing, "scope-a"), None);
        let corrupt = tmp.path().join("bad.json");
        std::fs::write(&corrupt, b"{ not json").unwrap();
        assert_eq!(active_session_at(&corrupt, "scope-a"), None, "corrupt must not panic");
    }

    #[test]
    fn default_namespace_shapes() {
        assert_eq!(default_namespace(Some("sess-1")), "scratchpad/sess-1/");
        assert_eq!(default_namespace(None), "scratchpad/");
        // Sanitized hook ids are namespace-legal by construction; the shape
        // must pass the MCP namespace validator.
        crate::tools::staging::validate_namespace(&default_namespace(Some("a-b.c_d")))
            .expect("per-session namespace must satisfy the validator");
    }

    #[test]
    fn pending_handover_fires_once_per_session_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_marker(&tmp, "scope-h1", "sess-A");
        // Fresh process + marker present -> one pending handover (restart-safe).
        assert!(take_pending_handover_at(&path, "scope-h1"), "first observation must fire");
        assert!(!take_pending_handover_at(&path, "scope-h1"), "same session must not re-fire");
        // Marker flips to a new session -> fires exactly once again.
        write_marker(&tmp, "scope-h1", "sess-B");
        assert!(take_pending_handover_at(&path, "scope-h1"), "session change must fire");
        assert!(!take_pending_handover_at(&path, "scope-h1"));
    }

    #[test]
    fn pending_handover_without_marker_never_fires() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        assert!(
            !take_pending_handover_at(&missing, "scope-h2"),
            "no marker (hook not installed) must never trigger handover work"
        );
    }
}
