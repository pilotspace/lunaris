//! Per-session scratchpad resolution + handover tracking (scratchpad-handover task).
//!
//! Read-side mirror of the `~/.lunaris/sessions.json` marker that
//! `lunaris-hook` maintains (session-switch-detect task). Same dual-impl
//! pattern as the two `JsonScopesFileStore`s: each binary owns its struct,
//! neither imports the other, both agree on the on-disk format.
//!
//! RED state: stubs only — the marker is never read, the default namespace
//! never rotates, and no handover fires. The tests below encode the frozen
//! contract and MUST fail until the build fills these in.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Test seam: overrides the sessions-file location for in-process tests
/// (mirrors the `SKIP_STAGE` atomic pattern — no `unsafe env::set_var`).
static SESSIONS_FILE_OVERRIDE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

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
/// `path`. `None` on missing file / missing entry / any IO or parse error.
pub(crate) fn active_session_at(_path: &Path, _scope: &str) -> Option<String> {
    None // RED stub
}

/// The default scratchpad namespace for an optional active session:
/// `scratchpad/{id}/` when present, `scratchpad/` otherwise.
pub(crate) fn default_namespace(_active: Option<&str>) -> String {
    "scratchpad/".to_owned() // RED stub
}

/// Returns `true` exactly once per (process, scope, active-session) change.
pub(crate) fn take_pending_handover_at(_path: &Path, _scope: &str) -> bool {
    false // RED stub
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
