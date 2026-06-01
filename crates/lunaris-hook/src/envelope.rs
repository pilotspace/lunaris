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
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| ParseError::InvalidJson(e.to_string()))?;

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
    DateTime::parse_from_rfc3339(ts_str).ok().map(|dt| dt.with_timezone(&Utc))
}

// ── HOOK-05: helpers needed by `run()` for the dedupe pipeline ────────────────

/// Parse an already-parsed `serde_json::Value` into a `HookEvent`.
///
/// Identical logic to [`parse`] but avoids re-serialising/re-parsing bytes
/// when the caller already holds a `Value`. Used by `run()` which first
/// deserialises the raw bytes into a `Value` for canonical-JSON hashing,
/// then wants the typed `HookEvent` for content extraction.
pub fn parse_value(value: &serde_json::Value) -> Result<HookEvent, ParseError> {
    let kind = value
        .get("hook_event_name")
        .and_then(|k| k.as_str())
        .ok_or_else(|| ParseError::MissingField("hook_event_name".into()))?
        .to_owned();

    match kind.as_str() {
        "PreToolUse" => {
            let p: PreToolUsePayload = serde_json::from_value(value.clone())
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::PreToolUse(p))
        }
        "PostToolUse" => {
            let p: PostToolUsePayload = serde_json::from_value(value.clone())
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::PostToolUse(p))
        }
        "Stop" => {
            let p: StopPayload = serde_json::from_value(value.clone())
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::Stop(p))
        }
        "SessionStart" => {
            let p: SessionStartPayload = serde_json::from_value(value.clone())
                .map_err(|e| ParseError::InvalidFields(kind.clone(), e.to_string()))?;
            Ok(HookEvent::SessionStart(p))
        }
        other => Ok(HookEvent::Unknown(other.to_owned())),
    }
}

/// Extract the kind-specific content string from a raw envelope `Value`.
///
/// | Kind          | Extracted field     |
/// |---------------|---------------------|
/// | PreToolUse    | `tool_input` (JSON) |
/// | PostToolUse   | `tool_response` (JSON) |
/// | Stop          | `""` (no content)   |
/// | SessionStart  | `""` (no content)   |
///
/// The field is serialised back to a compact JSON string so that the scrubber
/// and dedupe pipeline operate on a flat string rather than a nested `Value`.
pub fn extract_content_string(event_value: &serde_json::Value) -> String {
    let kind = event_value.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("");

    match kind {
        "PreToolUse" => event_value
            .get("tool_input")
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default(),
        "PostToolUse" => event_value
            .get("tool_response")
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default(),
        // Stop, SessionStart, Unknown: no payload content to extract.
        _ => String::new(),
    }
}

/// Write the scrubbed content string back into the relevant field of `event_value`
/// so that [`crate::dedupe::canonical_json`] hashes the post-scrub form.
///
/// For `PreToolUse`: replaces `tool_input` with the scrubbed string (re-parsed as
/// a JSON value; falls back to a plain JSON string if re-parsing fails).
/// For `PostToolUse`: replaces `tool_response` similarly.
/// For other kinds: no-op (no content field to splice).
pub fn splice_content_string(event_value: &mut serde_json::Value, content: &str) {
    let kind = event_value.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("").to_owned();

    // Try to parse the scrubbed content back as JSON; fall back to a bare string value.
    let content_value: serde_json::Value = serde_json::from_str(content)
        .unwrap_or_else(|_| serde_json::Value::String(content.to_owned()));

    match kind.as_str() {
        "PreToolUse" => {
            if let Some(obj) = event_value.as_object_mut() {
                obj.insert("tool_input".to_string(), content_value);
            }
        }
        "PostToolUse" => {
            if let Some(obj) = event_value.as_object_mut() {
                obj.insert("tool_response".to_string(), content_value);
            }
        }
        _ => {} // Stop, SessionStart, Unknown: nothing to splice.
    }
}

/// Extract the `event_id` field from a typed `HookEvent`, if present.
pub fn extract_event_id(event: &HookEvent) -> Option<String> {
    match event {
        HookEvent::PreToolUse(p) => p.event_id.clone(),
        HookEvent::PostToolUse(p) => p.event_id.clone(),
        HookEvent::Stop(p) => p.event_id.clone(),
        HookEvent::SessionStart(p) => p.event_id.clone(),
        HookEvent::Unknown(_) => None,
    }
}

/// Extract the `session_id` field from a typed `HookEvent`.
/// Returns `""` for Unknown events (should not occur in practice — Unknown is
/// short-circuited before this helper is called).
pub fn extract_session_id(event: &HookEvent) -> &str {
    match event {
        HookEvent::PreToolUse(p) => &p.session_id,
        HookEvent::PostToolUse(p) => &p.session_id,
        HookEvent::Stop(p) => &p.session_id,
        HookEvent::SessionStart(p) => &p.session_id,
        HookEvent::Unknown(_) => "",
    }
}

/// Extract the hook event name string (the kind discriminator) from a typed `HookEvent`.
pub fn extract_kind_str(event: &HookEvent) -> &str {
    match event {
        HookEvent::PreToolUse(p) => &p.hook_event_name,
        HookEvent::PostToolUse(p) => &p.hook_event_name,
        HookEvent::Stop(p) => &p.hook_event_name,
        HookEvent::SessionStart(p) => &p.hook_event_name,
        HookEvent::Unknown(s) => s.as_str(),
    }
}

/// Extract the `transcript_path` field from a typed `HookEvent`, if present.
pub fn extract_transcript_path(event: &HookEvent) -> Option<&str> {
    match event {
        HookEvent::PreToolUse(p) => p.transcript_path.as_deref(),
        HookEvent::PostToolUse(p) => p.transcript_path.as_deref(),
        HookEvent::Stop(p) => p.transcript_path.as_deref(),
        HookEvent::SessionStart(p) => p.transcript_path.as_deref(),
        HookEvent::Unknown(_) => None,
    }
}

/// Extract the raw `timestamp` string from a typed `HookEvent`, if present.
/// (Counterpart to `extract_timestamp` which parses into `DateTime<Utc>`.)
pub fn extract_timestamp_str(event: &HookEvent) -> Option<&str> {
    match event {
        HookEvent::PreToolUse(p) => p.timestamp.as_deref(),
        HookEvent::PostToolUse(p) => p.timestamp.as_deref(),
        HookEvent::Stop(p) => p.timestamp.as_deref(),
        HookEvent::SessionStart(p) => p.timestamp.as_deref(),
        HookEvent::Unknown(_) => None,
    }
}
