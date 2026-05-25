//! Hook policy TOML schema (locked v0).
//!
//! `~/.lunaris/hook-policy.toml` configures extra scrubber patterns and path
//! filters.  A missing file is a silent fallback — no error, no warning.
//! A malformed file emits a `warn!` log to stderr and falls back to built-ins
//! only.  The TOML overlay can only **add** patterns; built-ins always run.

use serde::Deserialize;

/// Root of `~/.lunaris/hook-policy.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HookPolicy {
    pub scrubbers: ScrubberPolicy,
    pub filters: FilterPolicy,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ScrubberPolicy {
    pub custom: CustomScrubbers,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CustomScrubbers {
    pub patterns: Vec<CustomPattern>,
}

/// A single user-defined scrubber pattern from the TOML overlay.
#[derive(Debug, Deserialize)]
pub struct CustomPattern {
    /// Human-readable name for this pattern (used in log messages).
    pub name: String,
    /// The regex pattern string.  Compiled once at engine construction.
    /// Patterns longer than 256 characters are silently skipped (ReDoS guard).
    pub pattern: String,
    /// The replacement string to use when this pattern matches.
    pub redact_as: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FilterPolicy {
    pub paths: PathPolicy,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PathPolicy {
    /// Additional glob patterns to exclude (added on top of built-in deny list).
    pub extra_excludes: Vec<String>,
    /// Explicit re-includes; built-in denies still win.
    pub extra_includes: Vec<String>,
}

impl HookPolicy {
    /// Load from the default path `~/.lunaris/hook-policy.toml`.
    ///
    /// Returns `None` (no error) if the file is absent.
    /// Returns `None` with a `warn!` log if the file exists but fails to parse.
    pub fn load_default() -> Option<Self> {
        let path = dirs::home_dir()?.join(".lunaris").join("hook-policy.toml");
        Self::load_from(&path)
    }

    /// Load from an explicit path (for testing and `from_toml_path`).
    ///
    /// Returns `None` silently if the file is not found.
    /// Returns `None` with a `warn!` log on any other I/O or parse error.
    pub fn load_from(path: &std::path::Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    err  = %e,
                    "hook-policy.toml read error — using built-ins only"
                );
                return None;
            }
        };
        match toml::from_str::<HookPolicy>(&text) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    err  = %e,
                    "hook-policy.toml parse error — using built-ins only"
                );
                None
            }
        }
    }
}
