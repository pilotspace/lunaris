//! Layered event-filter policy for `lunaris-hook` (HOOK-03).
//!
//! Filter ordering (per CONTEXT.md): path → kind → size.
//! Path-glob deny short-circuits first (cheapest). Kind next.
//! Content truncation last — operates on filtered payloads only.
//!
//! Built-in path deny list ALWAYS wins; `LUNARIS_HOOK_INCLUDE` cannot
//! re-allow a path that matches a built-in deny entry.
//!
//! # Exit code mapping
//!
//! `FilterVerdict::Deny` maps to exit code 66 in `main.rs` via
//! `HookError::Filtered`. (Wiring into `run()` happens in Plan 24-03.)

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Built-in path deny glob patterns.
/// These ALWAYS run; user overrides cannot remove them.
const BUILTIN_PATH_DENY: &[&str] = &["**/.env", "**/*.pem", "**/id_rsa*", "**/.git/**"];

/// Tool kinds captured by default.
const DEFAULT_ALLOW_KINDS: &[&str] = &["Read", "Edit", "MultiEdit", "Write", "Bash"];

/// Maximum payload size before truncation (128 KiB).
const MAX_PAYLOAD_BYTES: usize = 128 * 1024;
/// Head preserved in a truncated payload (64 KiB).
const TRUNCATION_HEAD: usize = 64 * 1024;
/// Tail preserved in a truncated payload (32 KiB).
const TRUNCATION_TAIL: usize = 32 * 1024;

/// Result of applying the filter to one event.
#[derive(Debug, PartialEq, Eq)]
pub enum FilterVerdict {
    /// Event passes all filters — proceed to scrub + ingest.
    Allow,
    /// Event rejected by path/kind/policy. Caller exits 66 (no Episode written).
    Deny,
}

/// Payload after possible content truncation.
#[derive(Debug)]
pub struct TruncatedPayload {
    pub content: String,
    /// Number of bytes elided (0 means no truncation occurred).
    pub truncated_bytes: u64,
}

/// Compiled filter policy derived from environment and built-ins.
#[derive(Debug)]
pub struct FilterPolicy {
    builtin_deny: GlobSet,
    user_exclude: GlobSet,
    user_include: GlobSet,
    allow_kinds: Vec<String>,
}

impl FilterPolicy {
    /// Build a `FilterPolicy` from the current process environment.
    ///
    /// Env vars:
    /// - `LUNARIS_HOOK_INCLUDE`: colon-separated glob patterns that EXTEND
    ///   the allow list (built-in denies still win).
    /// - `LUNARIS_HOOK_EXCLUDE`: colon-separated glob patterns that EXTEND
    ///   the path deny list.
    pub fn from_env() -> Result<Self, FilterError> {
        let builtin_deny = build_globset(BUILTIN_PATH_DENY)?;

        let user_exclude = {
            let raw = std::env::var("LUNARIS_HOOK_EXCLUDE").unwrap_or_default();
            let patterns: Vec<&str> = raw.split(':').filter(|s| !s.is_empty()).collect();
            build_globset(&patterns)?
        };

        let user_include = {
            let raw = std::env::var("LUNARIS_HOOK_INCLUDE").unwrap_or_default();
            let patterns: Vec<&str> = raw.split(':').filter(|s| !s.is_empty()).collect();
            build_globset(&patterns)?
        };

        let allow_kinds = DEFAULT_ALLOW_KINDS.iter().map(|s| s.to_string()).collect();

        Ok(Self { builtin_deny, user_exclude, user_include, allow_kinds })
    }

    /// Apply the three-layer filter policy to a hook event.
    ///
    /// Returns `FilterVerdict::Deny` if the event should be dropped.
    pub fn apply(&self, event: &crate::envelope::HookEvent) -> FilterVerdict {
        // Extract path from tool_input if present.
        let path = extract_path(event);
        let tool_name = extract_tool_name(event);

        // Layer 1 — path glob filter.
        if let Some(p) = path {
            // Built-in deny ALWAYS wins, even against explicit INCLUDE.
            if self.builtin_deny.is_match(p) {
                return FilterVerdict::Deny;
            }
            // User exclude extends the deny list.
            // INCLUDE can override user_exclude, but NOT builtin_deny (checked above).
            if self.user_exclude.is_match(p) && !self.user_include.is_match(p) {
                return FilterVerdict::Deny;
            }
        }

        // Layer 2 — event kind filter.
        if let Some(kind) = tool_name {
            let allowed_by_default = self.allow_kinds.iter().any(|k| k == kind);
            if !allowed_by_default {
                // Check if user explicitly includes this path pattern to override kind filter.
                let path_included = path.map(|p| self.user_include.is_match(p)).unwrap_or(false);
                if !path_included {
                    return FilterVerdict::Deny;
                }
            }
        }

        // Unknown / Stop / SessionStart carry no tool_name — always allowed.
        FilterVerdict::Allow
    }

    /// Truncate a string payload if it exceeds 128 KiB.
    ///
    /// Truncation format: `<head:64KiB>…elided…<tail:32KiB>`
    ///
    /// Returns the (possibly truncated) content and the number of bytes elided.
    pub fn truncate_payload(content: &str) -> TruncatedPayload {
        let bytes = content.as_bytes();
        if bytes.len() <= MAX_PAYLOAD_BYTES {
            return TruncatedPayload { content: content.to_owned(), truncated_bytes: 0 };
        }

        let head = std::str::from_utf8(&bytes[..TRUNCATION_HEAD]).unwrap_or_default().to_owned();
        let tail_start = bytes.len().saturating_sub(TRUNCATION_TAIL);
        let tail = std::str::from_utf8(&bytes[tail_start..]).unwrap_or_default().to_owned();
        let elided = (bytes.len() - TRUNCATION_HEAD - TRUNCATION_TAIL) as u64;

        TruncatedPayload {
            content: format!("{head}\u{2026}elided\u{2026}{tail}"),
            truncated_bytes: elided,
        }
    }
}

/// Errors from `FilterPolicy` construction.
#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("invalid glob pattern: {0}")]
    InvalidGlob(String),
}

fn build_globset<S: AsRef<str>>(patterns: &[S]) -> Result<GlobSet, FilterError> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p.as_ref()).map_err(|e| FilterError::InvalidGlob(e.to_string()))?;
        builder.add(glob);
    }
    builder.build().map_err(|e| FilterError::InvalidGlob(e.to_string()))
}

/// Extract a filesystem path from `tool_input.path` (if present) on Pre/PostToolUse events.
fn extract_path(event: &crate::envelope::HookEvent) -> Option<&str> {
    match event {
        crate::envelope::HookEvent::PreToolUse(p) => {
            p.tool_input.get("path").and_then(|v| v.as_str())
        }
        crate::envelope::HookEvent::PostToolUse(p) => {
            p.tool_input.get("path").and_then(|v| v.as_str())
        }
        _ => None,
    }
}

/// Extract the tool name (kind discriminator) for kind-layer filtering.
fn extract_tool_name(event: &crate::envelope::HookEvent) -> Option<&str> {
    match event {
        crate::envelope::HookEvent::PreToolUse(p) => Some(&p.tool_name),
        crate::envelope::HookEvent::PostToolUse(p) => Some(&p.tool_name),
        _ => None,
    }
}
