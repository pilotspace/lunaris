//! Build an [`EpisodeBuilder`] from a parsed [`HookEvent`].
//!
//! INGEST-04 invariant: this module MUST NOT call `atomic_write` directly.
//! The single `atomic_write` per ingest lives in `ScopedLunaris::ingest`.
//! `grep -c 'atomic_write' crates/lunaris-hook/src/ingest.rs` MUST return 0.

use chrono::Utc;
use lunaris::EpisodeBuilder;
use serde_json::{Map, Value};

use crate::envelope::{HookEvent, extract_timestamp};

/// Build an [`EpisodeBuilder`] from a parsed hook event.
///
/// Returns `None` for [`HookEvent::Unknown`] — callers exit 0 without
/// calling `ScopedLunaris::ingest`.
pub fn build_episode(event: &HookEvent) -> Option<EpisodeBuilder> {
    let (source, content, meta) = match event {
        HookEvent::PreToolUse(p) => {
            let source = "claude-code:pre_tool_use".to_string();
            let content = format!(
                "tool_input: {}",
                serde_json::to_string(&p.tool_input).unwrap_or_default()
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
