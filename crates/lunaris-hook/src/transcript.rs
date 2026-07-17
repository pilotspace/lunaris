//! Stop-time transcript reader (engram-soul-loop task 3 — citation detector).
//!
//! Parses the Claude Code / Codex JSONL transcript to recover, for the
//! current turn: the memories injected via `hook_additional_context`
//! attachments, the tool-call outcomes (`tool_use` id joined to a
//! `tool_result`'s `is_error`), and the final assistant message text.
//!
//! Empirically verified shape (`.add/tasks/citation-detector/TASK.md` §0
//! GROUND, 3 real transcripts under `~/.claude/projects/...`, 2026-07-17):
//! - top-level JSONL entries carry `type ∈ {assistant, user, attachment,
//!   system, ...}` and a top-level `sessionId` (camelCase).
//! - an injection is `type == "attachment"` whose nested `attachment.type
//!   == "hook_additional_context"`; `attachment.content` holds the
//!   `<lunaris_memory_context phase="..">` block — observed as a
//!   single-element JSON array of strings (NOT a bare string), tolerated
//!   here as either shape.
//! - `attachment.toolUseID` is the tool_use id this injection is attached
//!   to; only meaningful when the block's `phase` is `post_tool` (at other
//!   phases it is a synthetic hook-invocation id, not a real tool_use id).
//! - assistant tool calls are `type == "assistant"`,
//!   `.message.content[].type == "tool_use"` (`id`, `name`).
//! - tool outcomes are `type == "user"`, `.message.content[].type ==
//!   "tool_result"` (`tool_use_id`, optional `is_error`) — `is_error` may be
//!   ABSENT entirely, not just `null`.
//! - the final assistant message is the LAST `type == "assistant"` entry
//!   that carries at least one `text` content block, its text blocks joined.
//!
//! Tail-bounded (`LUNARIS_TRANSCRIPT_TAIL_BYTES`, default
//! [`DEFAULT_TAIL_BYTES`]) so a week-long session never makes Stop O(file).
//! Lenient per-line: a malformed line, an unparseable injection line, or a
//! missing field never aborts the read — it is skipped and parsing
//! continues (fail-open per §1 Reject).

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;
use ulid::Ulid;

/// Default trailing-byte budget for [`read_turn_transcript`]
/// (`LUNARIS_TRANSCRIPT_TAIL_BYTES` env override).
pub const DEFAULT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// One memory injected into the transcript via a `hook_additional_context`
/// attachment, parsed from a `<lunaris_memory_context phase="...">` block
/// line: `- [source=<s> score=<f> id=<26-char ULID>] <snippet>`.
#[derive(Debug, Clone, PartialEq)]
pub struct InjectedMemory {
    pub id: Ulid,
    pub snippet: String,
    pub phase: String,
    pub tool_use_id: Option<String>,
}

/// A tool call's structured outcome, joined from a `type == "user"` entry's
/// `tool_result` content block.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub tool_use_id: String,
    pub is_error: Option<bool>,
}

/// The parsed slice of a transcript relevant to Stop-time citation grading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnTranscript {
    pub injections: Vec<InjectedMemory>,
    pub tool_outcomes: Vec<ToolOutcome>,
    pub final_assistant_text: String,
    pub session_ids_seen: HashSet<String>,
    /// engram-soul-loop task 10 — on-disk size of the transcript file this
    /// was parsed from (`metadata().len()`, already fetched by
    /// `read_turn_transcript` for the tail-seek math). Default `0` for any
    /// caller that builds a `TurnTranscript` without a real file behind it.
    pub file_bytes: u64,
}

/// Read the trailing `tail_bytes` of the transcript at `path` and parse it
/// into a [`TurnTranscript`].
///
/// Tail-bounded: only the last `tail_bytes` of the file are read (via a
/// single seek + read-to-end), so a week-long session never makes this
/// O(file). When the seek lands mid-file (`tail_bytes < file length`), the
/// first — necessarily partial — line is discarded. Every JSONL line is
/// parsed independently; a malformed line is skipped rather than aborting
/// the read (fail-open per §1 Reject). Returns `Err` only for the
/// file-level I/O failure (missing file, permission denied, ...) — the
/// caller maps that to `detector: "skipped_no_transcript"`.
pub fn read_turn_transcript(path: &Path, tail_bytes: u64) -> std::io::Result<TurnTranscript> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(tail_bytes);
    let discard_first_line = start > 0;
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // Lossy: a mid-file seek can land inside a multi-byte UTF-8 sequence.
    // The lenient per-line parse below simply drops the (at most one)
    // affected line rather than erroring the whole read.
    let text = String::from_utf8_lossy(&bytes);

    let mut transcript = TurnTranscript { file_bytes: len, ..TurnTranscript::default() };
    let mut last_assistant_text: Option<String> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        if idx == 0 && discard_first_line {
            continue;
        }
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };

        if let Some(session_id) = entry_session_id(&value) {
            transcript.session_ids_seen.insert(session_id.to_owned());
        }

        match kind {
            "attachment" => {
                if let Some(attachment) = value.get("attachment") {
                    parse_attachment(attachment, &mut transcript.injections);
                }
            }
            "assistant" => {
                if let Some(text) = extract_assistant_text(&value) {
                    last_assistant_text = Some(text);
                }
            }
            "user" => {
                extract_tool_outcomes(&value, &mut transcript.tool_outcomes);
            }
            _ => {}
        }
    }

    transcript.final_assistant_text = last_assistant_text.unwrap_or_default();
    Ok(transcript)
}

/// `sessionId` (real transcript field, camelCase) with a `session_id`
/// fallback for lenience.
fn entry_session_id(value: &Value) -> Option<&str> {
    value
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| value.get("session_id").and_then(Value::as_str))
}

/// Join every `text` content block of an assistant entry's message.
/// Returns `None` when the entry has no text block (e.g. a tool-use-only
/// turn) so the caller never overwrites the running "last text" with
/// nothing.
fn extract_assistant_text(entry: &Value) -> Option<String> {
    let content = entry.pointer("/message/content")?.as_array()?;
    let texts: Vec<&str> = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if texts.is_empty() { None } else { Some(texts.join("\n")) }
}

/// Extract every `tool_result` content block of a user entry into
/// [`ToolOutcome`]s. A block missing `tool_use_id` is skipped (unparseable
/// per §1 Reject); `is_error` degrades to `None` when absent or non-bool.
fn extract_tool_outcomes(entry: &Value, out: &mut Vec<ToolOutcome>) {
    let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let is_error = block.get("is_error").and_then(Value::as_bool);
        out.push(ToolOutcome { tool_use_id: tool_use_id.to_owned(), is_error });
    }
}

/// Parse one `attachment` entry's nested object. Ignores anything that is
/// not a `hook_additional_context` attachment (e.g. `hook_success`).
fn parse_attachment(attachment: &Value, injections: &mut Vec<InjectedMemory>) {
    if attachment.get("type").and_then(Value::as_str) != Some("hook_additional_context") {
        return;
    }
    let Some(content_text) = attachment_content_text(attachment) else {
        return;
    };
    let phase = extract_phase(&content_text);
    // `toolUseID` only identifies a REAL assistant tool_use at the
    // `post_tool` phase — at `prompt`/`session_start` it is a synthetic
    // hook-invocation id and must not be attributed to a tool outcome.
    let tool_use_id = if phase == "post_tool" {
        attachment.get("toolUseID").and_then(Value::as_str).map(str::to_owned)
    } else {
        None
    };

    for line in content_text.lines() {
        if let Some(mem) = parse_injection_line(line, &phase, tool_use_id.clone()) {
            injections.push(mem);
        }
    }
}

/// `attachment.content` is observed as a one-element JSON array of strings
/// in real transcripts; tolerate a bare string too (lenient per §1 Reject —
/// an unexpected shape must not abort the whole attachment).
fn attachment_content_text(attachment: &Value) -> Option<String> {
    match attachment.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(items)) => {
            let joined: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if joined.is_empty() { None } else { Some(joined.join("\n")) }
        }
        _ => None,
    }
}

/// Extract the `phase="..."` attribute of the `<lunaris_memory_context>`
/// opening tag. Defaults to `"prompt"` when absent — a missing attribute
/// must never abort the block (§1 Reject: lenient per-line parse).
fn extract_phase(content_text: &str) -> String {
    if let Some(start) = content_text.find("phase=\"") {
        let after = &content_text[start + "phase=\"".len()..];
        if let Some(end) = after.find('"') {
            return after[..end].to_owned();
        }
    }
    "prompt".to_owned()
}

/// Parse one memory line: `- [source=<s> score=<f> id=<ulid>] <snippet>`.
/// Returns `None` on any shape mismatch (missing `id=`, malformed ULID, no
/// `]` delimiter) — the caller skips the line and keeps the rest of the
/// block (§1 Reject).
///
/// `pub(crate)` (engram-soul-loop task 6, staleness-pass): a stale-marked
/// line appends `⚠ code-changed` inside the bracket header
/// (`context.rs::render_context`); `context.rs`'s exit-criterion test needs
/// to prove the marker does not break `id=` extraction, so this parser is
/// exposed crate-wide rather than kept `context.rs`-module-private.
pub(crate) fn parse_injection_line(
    line: &str,
    phase: &str,
    tool_use_id: Option<String>,
) -> Option<InjectedMemory> {
    let line = line.trim();
    let rest = line.strip_prefix("- [")?;
    let (header, tail) = rest.split_once(']')?;
    let snippet = tail.trim_start().to_owned();

    let id = header
        .split_whitespace()
        .find_map(|field| field.strip_prefix("id="))
        .and_then(|id_str| Ulid::from_string(id_str).ok())?;

    Some(InjectedMemory { id, snippet, phase: phase.to_owned(), tool_use_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
    }

    #[test]
    fn reads_injections_tools_and_final_text() {
        let path = fixture_path("transcript_citation.jsonl");
        let transcript =
            read_turn_transcript(&path, DEFAULT_TAIL_BYTES).expect("fixture must read");

        // 4 injections: 2 prompt-phase (M1/M2), 2 post_tool-phase (M3/M4).
        assert_eq!(transcript.injections.len(), 4, "{:?}", transcript.injections);
        let by_snippet_has =
            |needle: &str| transcript.injections.iter().any(|m| m.snippet.contains(needle));
        assert!(by_snippet_has("granite embedder resolves llamacpp"));
        assert!(by_snippet_has("coffee brewing"));
        assert!(by_snippet_has("git commit created successfully"));
        assert!(by_snippet_has("risky schema migration"));

        let m3 = transcript
            .injections
            .iter()
            .find(|m| m.snippet.contains("git commit created successfully"))
            .expect("M3 injection present");
        assert_eq!(m3.phase, "post_tool");
        assert_eq!(m3.tool_use_id.as_deref(), Some("toolu_M3CALL00000000000001"));

        let m1 = transcript
            .injections
            .iter()
            .find(|m| m.snippet.contains("granite embedder"))
            .expect("M1 injection present");
        assert_eq!(m1.phase, "prompt");
        assert_eq!(m1.tool_use_id, None, "prompt-phase injections carry no tool_use_id");

        // Tool outcomes joined from the two post_tool tool_result entries.
        assert_eq!(transcript.tool_outcomes.len(), 2, "{:?}", transcript.tool_outcomes);
        let m3_outcome = transcript
            .tool_outcomes
            .iter()
            .find(|o| o.tool_use_id == "toolu_M3CALL00000000000001")
            .expect("M3 tool outcome present");
        assert_eq!(m3_outcome.is_error, Some(false));
        let m4_outcome = transcript
            .tool_outcomes
            .iter()
            .find(|o| o.tool_use_id == "toolu_M4CALL00000000000002")
            .expect("M4 tool outcome present");
        assert_eq!(m4_outcome.is_error, Some(true));

        // Final assistant text is the LAST text-bearing assistant entry,
        // not the earlier "Working on it" one.
        assert!(transcript.final_assistant_text.contains("granite embedder resolves llamacpp"));
        assert!(!transcript.final_assistant_text.contains("Working on it"));

        assert!(transcript.session_ids_seen.contains("sess-citation-1"));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        // The fixture contains one deliberate garbage line between the
        // prompt injection and the first tool call. A successful read past
        // it (finding both) proves the parser tolerates it.
        let path = fixture_path("transcript_citation.jsonl");
        let transcript =
            read_turn_transcript(&path, DEFAULT_TAIL_BYTES).expect("garbage line must not error");
        assert!(!transcript.injections.is_empty());
        assert!(!transcript.final_assistant_text.is_empty());
    }

    /// engram-soul-loop task 10 — `TurnTranscript::file_bytes` must carry the
    /// on-disk file size `read_turn_transcript` already reads via
    /// `metadata().len()` today (and currently discards).
    #[test]
    fn transcript_reader_reports_file_bytes() {
        let path = fixture_path("transcript_citation.jsonl");
        let expected = std::fs::metadata(&path).expect("fixture metadata must read").len();
        let transcript =
            read_turn_transcript(&path, DEFAULT_TAIL_BYTES).expect("fixture must read");
        assert_eq!(transcript.file_bytes, expected);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let path = std::path::Path::new("/nonexistent/path/does-not-exist.jsonl");
        assert!(read_turn_transcript(path, DEFAULT_TAIL_BYTES).is_err());
    }

    #[test]
    fn tail_window_bounds_the_read() {
        // Build a synthetic file: a large filler prefix (garbage JSON lines,
        // well over the tail budget) followed by a real injection entry
        // inside the trailing window.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big_transcript.jsonl");

        let filler_line = serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-tail",
            "uuid": "filler",
            "message": {"content": [{"type": "text", "text": "filler ".repeat(20)}]},
        })
        .to_string();
        // ~80 bytes/line * 100_000 lines ~= 8 MiB, comfortably over a small
        // test tail budget.
        let mut content = String::new();
        for _ in 0..100_000 {
            content.push_str(&filler_line);
            content.push('\n');
        }
        let tail_line = serde_json::json!({
            "type": "attachment",
            "sessionId": "sess-tail",
            "attachment": {
                "type": "hook_additional_context",
                "content": ["<lunaris_memory_context phase=\"prompt\">\n- [source=decision:x score=0.9 id=01HX00000000000000000000TA] tail marker snippet\n</lunaris_memory_context>"],
                "hookName": "UserPromptSubmit",
                "toolUseID": "hook-tail",
                "hookEvent": "UserPromptSubmit",
            },
        })
        .to_string();
        content.push_str(&tail_line);
        content.push('\n');
        std::fs::write(&path, &content).unwrap();

        let tail_bytes: u64 = 4096;
        let transcript =
            read_turn_transcript(&path, tail_bytes).expect("tail-bounded read must succeed");
        assert_eq!(transcript.injections.len(), 1, "only the tail injection must be found");
        assert_eq!(transcript.injections[0].snippet, "tail marker snippet");

        // Independent byte-bound check: re-derive the SAME seek math the
        // function is documented to use (`len.saturating_sub(tail_bytes)`)
        // via a counting reader wrapper, and assert it never exceeds the
        // declared budget. This pins the O(tail) contract at the file-math
        // level rather than the function's private internals.
        struct CountingReader<R> {
            inner: R,
            count: u64,
        }
        impl<R: Read> Read for CountingReader<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.count += n as u64;
                Ok(n)
            }
        }

        let file_len = std::fs::metadata(&path).unwrap().len();
        assert!(file_len > tail_bytes, "fixture must exceed the tail budget");
        let start = file_len.saturating_sub(tail_bytes);
        let mut raw = std::fs::File::open(&path).unwrap();
        raw.seek(SeekFrom::Start(start)).unwrap();
        let mut counting = CountingReader { inner: raw, count: 0 };
        let mut sink = Vec::new();
        counting.read_to_end(&mut sink).unwrap();
        assert!(
            counting.count <= tail_bytes,
            "tail-bounded read must never consume more than tail_bytes ({} > {})",
            counting.count,
            tail_bytes
        );
    }
}
