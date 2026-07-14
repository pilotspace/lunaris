//! LLM-optimized snippet rendering for stored episode content.
//!
//! Stored episodes are often JSON envelopes (hook captures, decision/edit
//! records) whose raw bytes waste the consuming model's tokens: smart-quote
//! sanitized keys, escaped quoting, alphabetical envelope noise before the
//! payload. This module renders them as compact one-line summaries
//! (`decision: …`, `edit path: …`, `tool output: …`, `prompt: …`).
//!
//! Shared by `lunaris-hook` (context injection curation) and `lunaris-mcp`
//! (`memory.recall` hit content) — RC-1 keyspace precedent: cross-crate
//! helpers live in `lunaris-core`, never as local copies.
//!
//! Behavior is pinned by the lunaris-hook curation unit tests (the original
//! home of this logic) plus this module's own tests.

use serde_json::{Map, Value};

/// Render a stored episode's content as a compact LLM-ready summary.
///
/// Returns `Some` only for recognized JSON payloads (possibly smart-quote
/// sanitized); `None` for plain text or unrecognized shapes — callers choose
/// their own fallback (context injection drops low-value text; recall falls
/// back to the single-line raw text).
pub fn summarize(source: &str, text: &str) -> Option<String> {
    let value = parse_jsonish(text)?;
    summarize_json(source, &value)
}

/// Parse text as JSON, tolerating the ingest sanitizer's smart quotes.
pub fn parse_jsonish(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok().or_else(|| {
        let normalized = text.replace(['“', '”'], "\"").replace(['‘', '’'], "'");
        serde_json::from_str(&normalized).ok()
    })
}

/// Source-aware summary of a parsed JSON payload.
pub fn summarize_json(source: &str, value: &Value) -> Option<String> {
    let object = value.as_object()?;
    // Nested lookups mirror string_field's trim-tolerance: smart-quote-scrubbed
    // episodes reparse with space-padded keys (`" tool_response "`), and an
    // exact `get` would drop the whole payload.
    if let Some(codex_payload) = object_field(object, "codex_payload") {
        return summarize_codex_payload(source, codex_payload);
    }
    if let Some(tool_input) = object_field(object, "tool_input")
        && let Some(summary) = summarize_json(source, tool_input)
    {
        return Some(summary);
    }
    if let Some(tool_response) = object_field(object, "tool_response")
        && let Some(summary) = summarize_json(source, tool_response)
    {
        return Some(summary);
    }

    if source.starts_with("decision:")
        && let Some(decision) = string_field(object, &["decision"])
    {
        let rationale = string_field(object, &["rationale"])
            .map(|value| format!("; rationale: {}", trim_to_chars(value, 120)))
            .unwrap_or_default();
        return Some(format!("decision: {decision}{rationale}"));
    }

    if source.starts_with("edit:") {
        let path = string_field(object, &["path", "file_path", "filePath"]);
        let body = string_field(object, &["intent", "after", "new_string", "content"]);
        return match (path, body) {
            (Some(path), Some(body)) => Some(format!("edit {path}: {}", trim_to_chars(body, 180))),
            (Some(path), None) => Some(format!("edit {path}")),
            (None, Some(body)) => Some(format!("edit: {}", trim_to_chars(body, 200))),
            (None, None) => None,
        };
    }

    let path = string_field(object, &["path", "file_path", "filePath"]);
    let new_string = string_field(object, &["new_string", "newString", "content", "note"]);
    let old_string = string_field(object, &["old_string", "oldString"]);
    let output = string_field(object, &["output", "result", "stderr", "error"]);
    let command = string_field(object, &["command"]);

    if let Some(path) = path {
        if let Some(new_string) = new_string {
            let prefix = if old_string.is_some() { "edit" } else { "tool input" };
            return Some(format!("{prefix} {path}: {}", trim_to_chars(new_string, 180)));
        }
        if source == "claude-code:pre_tool_use" || source == "codex:tool_call:pre" {
            return None;
        }
        return Some(format!("tool touched {path}"));
    }
    if let Some(output) = output {
        return Some(format!("tool output: {}", trim_to_chars(output, 200)));
    }
    if let Some(command) = command {
        return Some(format!("command: {}", trim_to_chars(command, 200)));
    }
    // Prompt captures carry their payload in `prompt` — without this branch a
    // recalled UserPromptSubmit episode renders as raw envelope JSON and the
    // snippet cap truncates before the payload. Lowest priority: any tool
    // path/output/command summary above wins.
    if let Some(prompt) = string_field(object, &["prompt"]) {
        return Some(format!("prompt: {}", trim_to_chars(prompt, 220)));
    }
    None
}

fn summarize_codex_payload(source: &str, value: &Value) -> Option<String> {
    if let Some(summary) = summarize_json(source, value) {
        return Some(summary);
    }
    let object = value.as_object()?;
    let tool = string_field(object, &["tool_name", "toolName", "name", "command"]);
    let path = string_field(object, &["path", "file_path", "filePath"]);
    let output = string_field(object, &["output", "result", "stderr", "error"]);
    match (tool, path, output) {
        (Some(tool), Some(path), Some(output)) => {
            Some(format!("{tool} {path}: {}", trim_to_chars(output, 160)))
        }
        (Some(tool), Some(path), None) => Some(format!("{tool} {path}")),
        (Some(tool), None, Some(output)) => Some(format!("{tool}: {}", trim_to_chars(output, 180))),
        (None, Some(path), Some(output)) => Some(format!("{path}: {}", trim_to_chars(output, 180))),
        (None, None, Some(output)) => Some(format!("tool output: {}", trim_to_chars(output, 200))),
        _ => None,
    }
}

/// Trim-tolerant object lookup: smart-quote-scrubbed keys carry pad spaces.
fn object_field<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    object.iter().find(|(key, _)| key.trim() == name).map(|(_, value)| value)
}

/// Trim-tolerant string field lookup over candidate key names, first match wins.
fn string_field<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| {
            object.iter().find(|(key, _)| key.trim() == *name).and_then(|(_, value)| value.as_str())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Trim to a character budget, marking elision.
pub fn trim_to_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    text.chars().take(max_chars.saturating_sub(14)).collect::<String>() + "\n[truncated]"
}

/// Collapse all whitespace runs to single spaces on one line.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_renders_curated() {
        let content = r#"{"decision":"adopt launchd","rationale":"KeepAlive restarts"}"#;
        let rendered = summarize("decision:x", content).expect("decision must summarize");
        assert_eq!(rendered, "decision: adopt launchd; rationale: KeepAlive restarts");
        assert!(!rendered.contains('{'), "no raw JSON in the summary");
    }

    #[test]
    fn smart_quote_codex_prompt_renders() {
        let content = "{ “ codex_hook_event_name ” : “ UserPromptSubmit ” , “ codex_payload ” :{ “ prompt ” : “ marker XR-1 ” } }";
        let rendered =
            summarize("claude-code:pre_tool_use", content).expect("prompt envelope must summarize");
        assert_eq!(rendered, "prompt: marker XR-1");
    }

    #[test]
    fn non_json_returns_none() {
        assert!(summarize("notes/a", "The cobalt gateway is CG-1.").is_none());
    }

    #[test]
    fn trim_and_single_line_pins() {
        assert_eq!(single_line("a\n  b\tc"), "a b c");
        assert_eq!(trim_to_chars("short", 200), "short");
        let long = "x".repeat(300);
        let trimmed = trim_to_chars(&long, 260);
        assert!(trimmed.chars().count() <= 260);
        assert!(trimmed.ends_with("[truncated]"));
    }
}
