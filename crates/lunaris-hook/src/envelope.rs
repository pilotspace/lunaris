//! Claude Code hook envelope types.
//!
//! # Deserialization strategy
//!
//! Two-pass dispatch:
//! 1. Parse stdin bytes as `serde_json::Value` — exit 64 on invalid JSON.
//! 2. Read `hook_event_name` string — exit 64 if missing.
//! 3. Match on the kind string:
//!    - Known kind → deserialize the typed payload struct (fields-leniently, no deny_unknown_fields).
//!    - Unknown kind → return `HookEvent::Unknown(kind)` → caller exits 0 (no-op).
//!
//! # Why no `deny_unknown_fields` on payload structs (B1 fix)
//!
//! Claude Code may add transient fields (e.g. `stop_hook_active`) at any time.
//! Unknown fields within a known event kind must be silently dropped. The kind-level
//! dispatch already provides forward-compat for new event types; payload-level
//! forward-compat is preserved by omitting `deny_unknown_fields` on the structs.
//!
//! `deny_unknown_fields` is reserved for Lunaris-owned internal DTOs (per CLAUDE.md
//! §"HTTP DTO discipline"). Claude Code envelopes are external, not Lunaris-owned.
//!
//! `tool_input` / `tool_response` are `serde_json::Value` — opaque blobs preserved
//! verbatim without modelling Anthropic's tool schemas.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// The deserialized result of parsing one stdin envelope.
#[derive(Debug)]
pub enum HookEvent {
    PreToolUse(PreToolUsePayload),
    PostToolUse(PostToolUsePayload),
    Stop(StopPayload),
    SessionStart(SessionStartPayload),
    /// Any hook_event_name not in the four known values.
    /// The raw kind string is preserved for the info-log line.
    Unknown(String),
}

/// Parse stdin bytes into a `HookEvent`.
///
/// Returns `Err(ParseError)` for malformed JSON or a missing
/// `hook_event_name` field. Returns `Ok(HookEvent::Unknown(...))` for
/// unrecognised event kinds (forward compatibility).
pub fn parse(bytes: &[u8]) -> Result<HookEvent, ParseError> {
    let v: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::InvalidJson(e.to_string()))?;

    let kind = v
        .get("hook_event_name")
        .and_then(|k| k.as_str())
        .ok_or_else(|| ParseError::MissingField("hook_event_name".into()))?
        .to_owned();

    match kind.as_str() {
        "PreToolUse" => {
            let p: PreToolUsePayload = serde_json::from_value(v)
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::PreToolUse(p))
        }
        "PostToolUse" => {
            let p: PostToolUsePayload = serde_json::from_value(v)
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::PostToolUse(p))
        }
        "Stop" => {
            let p: StopPayload = serde_json::from_value(v)
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::Stop(p))
        }
        "SessionStart" => {
            let p: SessionStartPayload = serde_json::from_value(v)
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::SessionStart(p))
        }
        other => Ok(HookEvent::Unknown(other.to_owned())),
    }
}

/// Errors returned by [`parse`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("invalid fields for event kind {0}: {1}")]
    InvalidFields(String, String),
}

// ── Per-variant payload structs ───────────────────────────────────────────────
//
// No `#[serde(deny_unknown_fields)]` — Claude Code may add transient fields
// (stop_hook_active, etc.) without breaking existing Lunaris binaries.
//
// `tool_input` / `tool_response` are `serde_json::Value` (opaque blobs).
// Fields dropped (replay-noise): stop_hook_active — ignored via absent deny_unknown_fields.

#[derive(Debug, Deserialize)]
pub struct PreToolUsePayload {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PostToolUsePayload {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default)]
    pub tool_response: serde_json::Value,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StopPayload {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionStartPayload {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// Extract the `timestamp` field from any known event variant, if present.
/// Returns `None` for Unknown events or when timestamp is absent.
pub fn extract_timestamp(event: &HookEvent) -> Option<DateTime<Utc>> {
    let ts_str = match event {
        HookEvent::PreToolUse(p) => p.timestamp.as_deref(),
        HookEvent::PostToolUse(p) => p.timestamp.as_deref(),
        HookEvent::Stop(p) => p.timestamp.as_deref(),
        HookEvent::SessionStart(p) => p.timestamp.as_deref(),
        HookEvent::Unknown(_) => None,
    }?;
    DateTime::parse_from_rfc3339(ts_str)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}
