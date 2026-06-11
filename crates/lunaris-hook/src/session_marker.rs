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

use std::collections::BTreeMap;
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

/// One scope's entry in the sessions file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MarkerEntry {
    active_session_id: String,
    ended: bool,
    updated_at: String,
}

type SessionsFile = BTreeMap<String, MarkerEntry>;

/// Sanitize a raw session id to the Scope alphabet `[A-Za-z0-9_\-.]`
/// (replacement char `-`) so downstream tasks can use it in namespaces and
/// keys without re-validating.
pub fn sanitize_session_id(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '-' })
        .collect()
}

/// Resolve the sessions file path: `LUNARIS_SESSIONS_FILE` override, else
/// `~/.lunaris/sessions.json` (sibling of `scopes.json`).
pub fn sessions_file_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LUNARIS_SESSIONS_FILE") {
        return PathBuf::from(p);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".lunaris").join("sessions.json")
}

/// Read the whole sessions file. ANY error (missing, unreadable, corrupt)
/// degrades to an empty map — warn on corruption, debug on absence.
fn load(path: &Path) -> SessionsFile {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(), err = %e,
                    "sessions file corrupt — treating as absent (will be replaced on next write)",
                );
                SessionsFile::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SessionsFile::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), err = %e, "sessions file unreadable — treating as absent");
            SessionsFile::new()
        }
    }
}

/// Atomic save (tmp + rename). Errors degrade to a warn — marker IO must
/// never change the hook's exit code.
fn save(path: &Path, map: &SessionsFile) {
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(map)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        tracing::warn!(path = %path.display(), err = %e, "sessions file write failed — switch detection degraded");
    }
}

/// Record `session_id` as active for `scope` and report a switch if a
/// DIFFERENT session was active. Crash-safe: works without any SessionEnd.
/// All IO errors degrade to `None` + warn.
pub fn observe_start_at(path: &Path, scope: &str, session_id: &str) -> Option<SwitchObserved> {
    let session_id = sanitize_session_id(session_id);
    let mut map = load(path);
    let switch = match map.get(scope) {
        Some(prev) if prev.active_session_id != session_id => Some(SwitchObserved {
            previous_session_id: prev.active_session_id.clone(),
            previous_ended: prev.ended,
        }),
        _ => None,
    };
    map.insert(
        scope.to_owned(),
        MarkerEntry {
            active_session_id: session_id,
            ended: false,
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    save(path, &map);
    switch
}

/// Mark `session_id` as cleanly ended for `scope`. A stale end (the marker
/// names a different active session) leaves the marker untouched (warn).
pub fn observe_end_at(path: &Path, scope: &str, session_id: &str) {
    let session_id = sanitize_session_id(session_id);
    let mut map = load(path);
    match map.get_mut(scope) {
        Some(entry) if entry.active_session_id == session_id => {
            entry.ended = true;
            entry.updated_at = chrono::Utc::now().to_rfc3339();
            save(path, &map);
        }
        Some(entry) => {
            tracing::warn!(
                scope,
                ended_session = %session_id,
                active_session = %entry.active_session_id,
                "stale SessionEnd — marker names a different active session, leaving it untouched",
            );
        }
        None => {
            tracing::warn!(scope, ended_session = %session_id, "SessionEnd without an active marker — ignored");
        }
    }
}

/// Read the active marker for `scope`: `(active_session_id, ended)`.
/// `None` on missing file, missing scope entry, or any IO/parse error.
pub fn read_active_at(path: &Path, scope: &str) -> Option<(String, bool)> {
    load(path).get(scope).map(|e| (e.active_session_id.clone(), e.ended))
}

/// Episode-metadata fields carried by a `session_start` episode when a
/// switch was observed. Consumed by [`crate::ingest`].
pub fn switch_meta(switch: &SwitchObserved) -> Vec<(String, serde_json::Value)> {
    vec![
        ("switch_from".to_owned(), serde_json::Value::String(switch.previous_session_id.clone())),
        ("switch_prev_ended".to_owned(), serde_json::Value::Bool(switch.previous_ended)),
    ]
}

/// Emit the single-line JSON stderr signal for an observed switch. This is
/// the machine-readable contract the scratchpad-handover task (and any
/// wrapper script) consumes; format mirrors the HOOK-06 emergency-drop line.
pub fn log_switch_line(scope: &str, switch: &SwitchObserved, new_session_id: &str) {
    let line = serde_json::json!({
        "level": "info",
        "event": "session_switch",
        "scope": scope,
        "from": switch.previous_session_id,
        "to": sanitize_session_id(new_session_id),
        "prev_ended": switch.previous_ended,
    });
    eprintln!("{line}");
}
