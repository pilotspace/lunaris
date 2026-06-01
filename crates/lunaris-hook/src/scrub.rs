//! Secret scrubber for `lunaris-hook` (HOOK-04).
//!
//! Built-in scrubber set (closed, auditable — no runtime configuration):
//!
//! | Kind       | Pattern                                              | Replacement           |
//! |------------|------------------------------------------------------|-----------------------|
//! | `ENV_KEY`  | `[A-Z_][A-Z0-9_]{2,31}=[^\s\n]{4,}`                 | `<REDACTED:ENV_KEY>`  |
//! | `AWS_KEY`  | `AKIA[0-9A-Z]{16}`                                   | `<REDACTED:AWS_KEY>`  |
//! | `GH_TOKEN` | `gh[pousx]_[A-Za-z0-9_]{36}`                         | `<REDACTED:GH_TOKEN>` |
//! | `SSH_KEY`  | `-----BEGIN [A-Z ]+ PRIVATE KEY-----`                 | `<REDACTED:SSH_KEY>`  |
//! | `JWT`      | `eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+` | `<REDACTED:JWT>` |
//!
//! User patterns (from `~/.lunaris/hook-policy.toml`) are **added on top**.
//! Built-ins always run regardless of user config.
//! A missing or malformed TOML file falls back to built-ins only — no error.
//!
//! ## Threat model notes (T-24-02-01 — ReDoS)
//!
//! All built-in patterns are linear-time (no nested quantifiers, no
//! backtracking ambiguity).  User-supplied patterns are compiled once at
//! engine construction (not per-call), so the blast radius of a slow pattern
//! is bounded to startup cost.  Additionally, any pattern string longer than
//! 256 characters is rejected at construction time (skipped with a `warn!`
//! log) to cap the surface area of catastrophic backtracking from adversarial
//! TOML files.
//!
//! ## Known false-positive risk (ENV_KEY pattern)
//!
//! The `ENV_KEY` pattern `[A-Z_][A-Z0-9_]{2,31}=[^\s\n]{4,}` is broad by
//! design — it catches `.env`-style lines.  Long uppercase environment
//! variables with values of ≥4 characters (e.g. `LUNARIS_HOOK_DROP_AFTER_MS=100ms`)
//! will be redacted even when their value is not a secret.  This is the
//! capture-today safety posture: erring on the side of over-redaction is
//! preferable to missing a real credential.  Operators who need to preserve
//! specific values should scope their hook to paths that don't contain such
//! settings.

use regex::Regex;

use crate::policy::HookPolicy;

/// Maximum byte length of a user-supplied regex pattern string.
/// Patterns exceeding this are skipped with a warn log (T-24-02-01 ReDoS guard).
const MAX_PATTERN_LEN: usize = 256;

/// Raw (pattern, replacement) pairs for the five built-in secret kinds.
/// Each entry is a linear-time regex — verified manually and in the test suite.
static BUILTIN_RAW: &[(&str, &str)] = &[
    // ENV_KEY: matches KEY=VALUE lines where value is ≥4 non-whitespace chars.
    (r"[A-Z_][A-Z0-9_]{2,31}=[^\s\n]{4,}", "<REDACTED:ENV_KEY>"),
    // AWS_KEY: 20-char AKIA… access key IDs.
    (r"AKIA[0-9A-Z]{16}", "<REDACTED:AWS_KEY>"),
    // GH_TOKEN: GitHub personal/OAuth/user/fine-grained tokens (ghp_, gho_, ghu_, ghs_, ghx_).
    (r"gh[pousx]_[A-Za-z0-9_]{36}", "<REDACTED:GH_TOKEN>"),
    // SSH_KEY: OpenSSH / PEM private key block headers.
    (r"-----BEGIN [A-Z ]+ PRIVATE KEY-----", "<REDACTED:SSH_KEY>"),
    // JWT: three Base64URL segments separated by dots, starting with eyJ (JSON header).
    (r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+", "<REDACTED:JWT>"),
];

/// A compiled scrubber engine.
///
/// Construct with [`ScrubEngine::new`] (built-ins only) or
/// [`ScrubEngine::from_policy`] / [`ScrubEngine::from_toml_path`] to add
/// user-defined overlay patterns on top of the built-ins.
pub struct ScrubEngine {
    /// Compiled `(regex, replacement)` pairs.  Built-in rules always appear
    /// first; user rules are appended after.
    rules: Vec<(Regex, String)>,
}

impl ScrubEngine {
    /// Compile the five built-in rules.
    fn builtin_rules() -> Vec<(Regex, String)> {
        BUILTIN_RAW
            .iter()
            .map(|(pat, repl)| {
                (
                    Regex::new(pat).expect("built-in scrubber pattern must be valid"),
                    repl.to_string(),
                )
            })
            .collect()
    }

    /// Construct an engine with built-ins only.
    pub fn new() -> Self {
        Self { rules: Self::builtin_rules() }
    }

    /// Construct an engine with built-ins **plus** custom patterns from a
    /// [`HookPolicy`].
    ///
    /// Invalid or overlong (>256 chars) custom patterns are skipped with a
    /// `warn!` log; they never prevent the built-ins from running.
    pub fn from_policy(policy: &HookPolicy) -> Self {
        let mut rules = Self::builtin_rules();
        for custom in &policy.scrubbers.custom.patterns {
            // T-24-02-01 ReDoS guard: reject patterns that are too long.
            if custom.pattern.len() > MAX_PATTERN_LEN {
                tracing::warn!(
                    name = %custom.name,
                    len  = custom.pattern.len(),
                    max  = MAX_PATTERN_LEN,
                    "custom scrubber pattern exceeds max length — skipping (ReDoS guard)"
                );
                continue;
            }
            match Regex::new(&custom.pattern) {
                Ok(re) => rules.push((re, custom.redact_as.clone())),
                Err(e) => tracing::warn!(
                    name = %custom.name,
                    err  = %e,
                    "custom scrubber pattern invalid — skipping"
                ),
            }
        }
        Self { rules }
    }

    /// Load policy from the default TOML path (`~/.lunaris/hook-policy.toml`)
    /// and construct an engine.  Falls back to built-ins on any error.
    pub fn from_default_policy() -> Self {
        match HookPolicy::load_default() {
            Some(policy) => Self::from_policy(&policy),
            None => Self::new(),
        }
    }

    /// Load policy from an explicit TOML path and construct an engine.
    /// Falls back to built-ins if the file is absent or fails to parse.
    ///
    /// This is the primary entry point for tests.
    pub fn from_toml_path(path: &std::path::Path) -> Self {
        match HookPolicy::load_from(path) {
            Some(policy) => Self::from_policy(&policy),
            None => Self::new(),
        }
    }

    /// Apply all scrubber rules to `content` in place.
    ///
    /// Rules are applied sequentially (built-ins first, then user rules).
    /// Each rule is applied globally — all matches within `content` are
    /// replaced in a single pass per rule.
    ///
    /// Returns the number of redactions applied (lower bound; may under-count
    /// when the same span is matched by multiple rules in sequence).
    pub fn apply(&self, content: &mut String) -> usize {
        let mut count = 0usize;
        for (re, replacement) in &self.rules {
            let replaced = re.replace_all(content, replacement.as_str());
            // `replace_all` returns `Cow::Borrowed` when nothing changed.
            if let std::borrow::Cow::Owned(new_val) = replaced {
                // Count matches on the *original* value before overwriting.
                count += re.find_iter(content).count().max(1);
                *content = new_val;
            }
        }
        count
    }
}

impl Default for ScrubEngine {
    fn default() -> Self {
        Self::new()
    }
}
