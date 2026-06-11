//! Build an [`EpisodeBuilder`] from a parsed [`HookEvent`].
//!
//! INGEST-04 invariant: this module MUST NOT call `atomic_write` directly.
//! The single `atomic_write` per ingest lives in `ScopedLunaris::ingest`.
//! `grep -c 'atomic_write' crates/lunaris-hook/src/ingest.rs` MUST return 0.

use chrono::Utc;
use lunaris::EpisodeBuilder;
use serde_json::{Map, Value};

use crate::envelope::{HookEvent, extract_timestamp};

#[derive(Debug)]
pub struct BuiltEpisodeParts {
    pub source: String,
    pub content: String,
    pub t_ref: chrono::DateTime<chrono::Utc>,
    pub metadata: Map<String, Value>,
}

// ── HOOK-05: error type ───────────────────────────────────────────────────────

/// Errors from [`build_episode_from_scrubbed`].
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// The event kind cannot produce an episode (only `Unknown` triggers this).
    #[error("cannot build episode from unknown hook event kind: {0}")]
    UnknownEventKind(String),
}

/// Build an [`EpisodeBuilder`] from a parsed hook event.
///
/// Returns `None` for [`HookEvent::Unknown`] — callers exit 0 without
/// calling `ScopedLunaris::ingest`.
pub fn build_episode(event: &HookEvent) -> Option<EpisodeBuilder> {
    let (source, content, meta) = match event {
        HookEvent::PreToolUse(p) => {
            let source = "claude-code:pre_tool_use".to_string();
            let content =
                format!("tool_input: {}", serde_json::to_string(&p.tool_input).unwrap_or_default());
            let meta = build_meta(
                &p.session_id,
                Some(&p.tool_name),
                p.event_id.as_deref(),
                &p.cwd,
                p.transcript_path.as_deref(),
                &p.hook_event_name,
            );
            (source, content, meta)
        }
        HookEvent::PostToolUse(p) => {
            let source = "claude-code:post_tool_use".to_string();
            let content = format!(
                "tool_input: {}\ntool_response: {}",
                serde_json::to_string(&p.tool_input).unwrap_or_default(),
                serde_json::to_string(&p.tool_response).unwrap_or_default(),
            );
            let meta = build_meta(
                &p.session_id,
                Some(&p.tool_name),
                p.event_id.as_deref(),
                &p.cwd,
                p.transcript_path.as_deref(),
                &p.hook_event_name,
            );
            (source, content, meta)
        }
        HookEvent::Stop(p) => {
            let source = "claude-code:stop".to_string();
            let content = "stop event".to_string();
            let meta = build_meta(
                &p.session_id,
                None,
                p.event_id.as_deref(),
                &p.cwd,
                p.transcript_path.as_deref(),
                &p.hook_event_name,
            );
            (source, content, meta)
        }
        HookEvent::SessionStart(p) => {
            let source = "claude-code:session_start".to_string();
            let content = "session_start event".to_string();
            let meta = build_meta(
                &p.session_id,
                None,
                p.event_id.as_deref(),
                &p.cwd,
                p.transcript_path.as_deref(),
                &p.hook_event_name,
            );
            (source, content, meta)
        }
        HookEvent::SessionEnd(p) => {
            let source = "claude-code:session_end".to_string();
            let content = "session_end event".to_string();
            let mut meta = build_meta(
                &p.session_id,
                None,
                p.event_id.as_deref(),
                p.cwd.as_deref().unwrap_or(""),
                p.transcript_path.as_deref(),
                &p.hook_event_name,
            );
            if let Some(reason) = &p.reason {
                meta.insert("reason".into(), Value::String(reason.clone()));
            }
            (source, content, meta)
        }
        HookEvent::Unknown(_) => return None,
    };

    let t_ref = extract_timestamp(event).unwrap_or_else(Utc::now);
    let mut builder = EpisodeBuilder::new(source, content);
    builder = builder.t_ref(t_ref);
    if !meta.is_empty() {
        builder = builder.metadata(meta);
    }
    Some(builder)
}

fn build_meta(
    session_id: &str,
    tool_name: Option<&str>,
    event_id: Option<&str>,
    cwd: &str,
    transcript_path: Option<&str>,
    hook_event_name: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("session_id".into(), Value::String(session_id.to_owned()));
    if let Some(tn) = tool_name {
        m.insert("tool_name".into(), Value::String(tn.to_owned()));
    }
    if let Some(eid) = event_id {
        m.insert("event_id".into(), Value::String(eid.to_owned()));
    }
    m.insert("cwd".into(), Value::String(cwd.to_owned()));
    if let Some(tp) = transcript_path {
        m.insert("transcript_path".into(), Value::String(tp.to_owned()));
    }
    m.insert("hook_event_name".into(), Value::String(hook_event_name.to_owned()));
    m
}

// ── HOOK-05: scrub-aware builder ──────────────────────────────────────────────

/// Build an [`EpisodeBuilder`] from a pre-scrubbed content string (HOOK-05).
///
/// Unlike [`build_episode`], which reads the content directly from the
/// parsed `HookEvent`, this function accepts a `scrubbed_content` string
/// that has already been through truncation and secret scrubbing. The
/// `event_value` is used only for metadata fields (session_id, kind,
/// cwd, transcript_path, event_id, timestamp) — its raw content fields
/// are intentionally ignored to ensure no un-scrubbed bytes reach storage.
///
/// Records `truncated_bytes` in the episode metadata under the key
/// `"truncated_bytes"` (value 0 means no truncation occurred).
///
/// Returns `Err(IngestError::UnknownEventKind)` if the envelope's
/// `hook_event_name` is absent or unrecognised — callers should have
/// already short-circuited on Unknown kinds before reaching this function.
///
/// INGEST-04 invariant: this function MUST NOT call `atomic_write`.
pub fn build_episode_from_scrubbed(
    event_value: &serde_json::Value,
    scrubbed_content: &str,
    truncated_bytes: u64,
) -> Result<EpisodeBuilder, IngestError> {
    let parts = build_episode_parts_from_scrubbed(event_value, scrubbed_content, truncated_bytes)?;
    let mut builder = EpisodeBuilder::new(parts.source, parts.content);
    builder = builder.t_ref(parts.t_ref);
    if !parts.metadata.is_empty() {
        builder = builder.metadata(parts.metadata);
    }
    Ok(builder)
}

pub fn build_episode_parts_from_scrubbed(
    event_value: &serde_json::Value,
    scrubbed_content: &str,
    truncated_bytes: u64,
) -> Result<BuiltEpisodeParts, IngestError> {
    let kind = event_value.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("");

    // Derive the source tag and extract metadata fields from the raw value.
    let (source, session_id, tool_name, event_id, cwd, transcript_path) = match kind {
        "PreToolUse" => {
            let session_id =
                event_value.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let tool_name =
                event_value.get("tool_name").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let event_id =
                event_value.get("event_id").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let cwd = event_value.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let transcript_path =
                event_value.get("transcript_path").and_then(|v| v.as_str()).map(|s| s.to_owned());
            (
                "claude-code:pre_tool_use".to_string(),
                session_id,
                tool_name,
                event_id,
                cwd,
                transcript_path,
            )
        }
        "PostToolUse" => {
            let session_id =
                event_value.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let tool_name =
                event_value.get("tool_name").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let event_id =
                event_value.get("event_id").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let cwd = event_value.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let transcript_path =
                event_value.get("transcript_path").and_then(|v| v.as_str()).map(|s| s.to_owned());
            (
                "claude-code:post_tool_use".to_string(),
                session_id,
                tool_name,
                event_id,
                cwd,
                transcript_path,
            )
        }
        "Stop" => {
            let session_id =
                event_value.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let cwd = event_value.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let event_id =
                event_value.get("event_id").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let transcript_path =
                event_value.get("transcript_path").and_then(|v| v.as_str()).map(|s| s.to_owned());
            ("claude-code:stop".to_string(), session_id, None, event_id, cwd, transcript_path)
        }
        "SessionStart" => {
            let session_id =
                event_value.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let cwd = event_value.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let event_id =
                event_value.get("event_id").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let transcript_path =
                event_value.get("transcript_path").and_then(|v| v.as_str()).map(|s| s.to_owned());
            (
                "claude-code:session_start".to_string(),
                session_id,
                None,
                event_id,
                cwd,
                transcript_path,
            )
        }
        "SessionEnd" => {
            let session_id =
                event_value.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            // cwd is optional on SessionEnd (may fire outside a workspace).
            let cwd = event_value.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let event_id =
                event_value.get("event_id").and_then(|v| v.as_str()).map(|s| s.to_owned());
            let transcript_path =
                event_value.get("transcript_path").and_then(|v| v.as_str()).map(|s| s.to_owned());
            (
                "claude-code:session_end".to_string(),
                session_id,
                None,
                event_id,
                cwd,
                transcript_path,
            )
        }
        other => {
            return Err(IngestError::UnknownEventKind(other.to_owned()));
        }
    };

    // Build metadata map (mirrors build_meta, plus truncated_bytes).
    let mut meta = Map::new();
    meta.insert("session_id".into(), Value::String(session_id));
    if let Some(tn) = tool_name {
        meta.insert("tool_name".into(), Value::String(tn));
    }
    if let Some(eid) = event_id {
        meta.insert("event_id".into(), Value::String(eid));
    }
    meta.insert("cwd".into(), Value::String(cwd));
    if let Some(tp) = transcript_path {
        meta.insert("transcript_path".into(), Value::String(tp));
    }
    meta.insert("hook_event_name".into(), Value::String(kind.to_owned()));
    if kind == "SessionEnd"
        && let Some(reason) = event_value.get("reason").and_then(|v| v.as_str())
    {
        meta.insert("reason".into(), Value::String(reason.to_owned()));
    }
    if truncated_bytes > 0 {
        meta.insert(
            "truncated_bytes".into(),
            Value::Number(serde_json::Number::from(truncated_bytes)),
        );
    }

    // Parse timestamp from the raw value (reuse the same RFC-3339 logic as extract_timestamp).
    let t_ref = event_value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(Utc::now);

    Ok(BuiltEpisodeParts { source, content: scrubbed_content.to_owned(), t_ref, metadata: meta })
}
