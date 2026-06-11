//! Durable last-active-session marker — `~/.lunaris/sessions.json`.
//!
//! lunaris-hook is a per-event short-lived process, so "which session was
//! active before this one?" must live on disk. The marker file is the ONLY
//! bridge to `lunaris-mcp`, which never receives the agent's `session_id`
//! (stdio tools have no session context); the scratchpad-handover task reads
//! this file from the MCP side to target the per-session pad.
//!
//! # Failure discipline (HOOK-06 alignment)
//!
//! Marker IO must NEVER change the hook's exit code or block the agent:
//! every read error degrades to "no marker" (warn), every write error
//! degrades to a warn. A corrupt file is treated as absent and replaced by
//! the next successful write.
//!
//! # File shape
//!
//! ```json
//! { "<scope>": { "active_session_id": "<sanitized>", "ended": false,
//!                "updated_at": "<rfc3339>" } }
//! ```
//!
//! Writes are atomic (tmp + rename), mirroring the `scopes.json` store
//! pattern in [`crate::scope`]. Override the location with
//! `LUNARIS_SESSIONS_FILE` (tests + non-default homes).

use std::path::{Path, PathBuf};

/// A session switch observed at `SessionStart`: a different session was
/// active (per the marker) when the new one began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchObserved {
    /// The previously-active (sanitized) session id.
    pub previous_session_id: String,
    /// Whether the previous session recorded a clean `SessionEnd`.
    pub previous_ended: bool,
}

/// Sanitize a raw session id to the Scope alphabet `[A-Za-z0-9_\-.]`
/// (replacement char `-`) so downstream tasks can use it in namespaces and
/// keys without re-validating.
pub fn sanitize_session_id(raw: &str) -> String {
    // RED-phase stub — green implements the alphabet mapping.
    raw.to_owned()
}

/// Resolve the sessions file path: `LUNARIS_SESSIONS_FILE` override, else
/// `~/.lunaris/sessions.json` (sibling of `scopes.json`).
pub fn sessions_file_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LUNARIS_SESSIONS_FILE") {
        return PathBuf::from(p);
    }
    // RED-phase stub — green mirrors scope::scopes_file_path's home handling.
    PathBuf::from("sessions.json")
}

/// Record `session_id` as active for `scope` and report a switch if a
/// DIFFERENT session was active. Crash-safe: works without any SessionEnd.
/// All IO errors degrade to `None` + warn.
pub fn observe_start_at(_path: &Path, _scope: &str, _session_id: &str) -> Option<SwitchObserved> {
    // RED-phase stub.
    None
}

/// Mark `session_id` as cleanly ended for `scope`. A stale end (the marker
/// names a different active session) leaves the marker untouched (warn).
pub fn observe_end_at(_path: &Path, _scope: &str, _session_id: &str) {
    // RED-phase stub.
}

/// Read the active marker for `scope`: `(active_session_id, ended)`.
/// `None` on missing file, missing scope entry, or any IO/parse error.
pub fn read_active_at(_path: &Path, _scope: &str) -> Option<(String, bool)> {
    // RED-phase stub.
    None
}

/// Episode-metadata fields carried by a `session_start` episode when a
/// switch was observed. Consumed by [`crate::ingest`].
pub fn switch_meta(_switch: &SwitchObserved) -> Vec<(String, serde_json::Value)> {
    // RED-phase stub.
    Vec::new()
}
