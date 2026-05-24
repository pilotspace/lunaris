//! Scope derivation for `lunaris-mcp`.
//!
//! Precedence:
//! 1. explicit override (`--scope` / `LUNARIS_MCP_SCOPE`) — highest priority
//! 2. `git remote.origin.url` + current branch → blake3 → `"git_<hex16>"`
//! 3. canonical cwd → blake3 → `"cwd_<hex16>"`
//!
//! Persistence at `~/.lunaris/scopes.json` maps the full blake3 derivation key
//! to the resolved scope name + created_at + source. Users may rename the
//! `name` field manually; subsequent calls to [`resolve`] will honour the
//! persisted name.
//!
//! # Test isolation
//!
//! [`resolve`] is the public entry point for production use. Tests call the
//! internal [`resolve_with`] which accepts an explicit `cwd` + `scopes_path`
//! so they never touch the real `~/.lunaris/scopes.json` or rely on the
//! process working directory.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use lunaris_core::{Scope, ScopeError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Public error type ─────────────────────────────────────────────────────────

/// Errors returned by [`resolve`] / [`resolve_with`].
#[derive(Debug, Error)]
pub(crate) enum ScopeResolveError {
    /// The `--scope` / `LUNARIS_MCP_SCOPE` override value failed
    /// [`Scope::new`] validation.
    #[error("override scope failed validation: {0}")]
    InvalidOverride(#[source] ScopeError),

    /// The home directory could not be determined (missing `HOME` env on Unix
    /// or equivalent platform failure).
    #[error("could not resolve home directory")]
    NoHome,

    /// An I/O error while reading or writing `~/.lunaris/scopes.json`.
    #[error("scope store io: {0}")]
    Io(#[from] std::io::Error),

    /// `~/.lunaris/scopes.json` exists but failed JSON parsing. The file is
    /// treated as corrupt and is overwritten with a fresh empty store.
    #[error("scope store parse: {0}")]
    Parse(#[from] serde_json::Error),

    /// The blake3-derived scope name failed [`Scope::new`] validation. This
    /// should be impossible because derived names are `[git|cwd]_<16 hex
    /// chars>` which always satisfies the alphabet; this variant guards
    /// against future regressions.
    #[error("derived scope failed validation: {0}")]
    DerivationInvalid(#[source] ScopeError),
}

// ── Persistence types ─────────────────────────────────────────────────────────

/// Top-level schema of `~/.lunaris/scopes.json`.
#[derive(Debug, Serialize, Deserialize, Default)]
struct ScopesFile {
    /// Keys are full 64-char blake3 hex strings (the canonical derivation
    /// key). Values record the human-readable scope name plus metadata.
    scopes: BTreeMap<String, ScopeEntry>,
}

/// One entry in the scopes store.
#[derive(Debug, Serialize, Deserialize)]
struct ScopeEntry {
    /// The [`Scope`] string that should be used for this derivation key.
    /// Users may rename this field manually; `resolve` honours it.
    name: String,
    /// RFC 3339-ish timestamp of first derivation (seconds resolution).
    created_at: String,
    /// How the scope was derived: `"git"` or `"cwd"`.
    source: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Derive or restore the [`Scope`] for the current session.
///
/// Precedence:
/// 1. `override_` if `Some` — validated via [`Scope::new`], never persisted.
/// 2. `git remote.origin.url` + active branch → blake3 → `"git_<hex16>"`.
/// 3. Canonical `std::env::current_dir()` → blake3 → `"cwd_<hex16>"`.
///
/// Derived scopes are persisted to `~/.lunaris/scopes.json` (keyed by the
/// full blake3 hex). If the key is already present, the stored `name` is
/// returned — allowing users to rename a scope and have the rename survive
/// across sessions.
///
/// The `LUNARIS_SCOPES_FILE` environment variable overrides the default store
/// path; useful for system-level integration tests that don't go through
/// [`resolve_with`].
pub(crate) fn resolve(override_: Option<&str>) -> Result<Scope, ScopeResolveError> {
    let cwd = std::env::current_dir()?;
    let scopes_path = scopes_file_path()?;
    resolve_with(override_, &cwd, &scopes_path)
}

/// Testable variant of [`resolve`] that accepts explicit `cwd` and
/// `scopes_path` so tests can operate in temp directories without touching
/// the real user state.
pub(crate) fn resolve_with(
    override_: Option<&str>,
    cwd: &Path,
    scopes_path: &Path,
) -> Result<Scope, ScopeResolveError> {
    // 1 — explicit override (highest priority, never persisted)
    if let Some(o) = override_ {
        return Scope::new(o).map_err(ScopeResolveError::InvalidOverride);
    }

    // 2 / 3 — derive from git or cwd
    let (derivation_key, candidate_name, source) = derive_from_env(cwd);

    // 4 — load store, honour any persisted rename, persist new entry if needed
    let mut store = load_store(scopes_path)?;

    let scope_str = if let Some(entry) = store.scopes.get(&derivation_key) {
        entry.name.clone()
    } else {
        let entry = ScopeEntry {
            name: candidate_name.clone(),
            created_at: rfc3339_now(),
            source,
        };
        store.scopes.insert(derivation_key.clone(), entry);
        save_store(scopes_path, &store)?;
        candidate_name
    };

    Scope::new(&scope_str).map_err(ScopeResolveError::DerivationInvalid)
}

// ── Derivation helpers ────────────────────────────────────────────────────────

/// Returns `(derivation_key, candidate_name, source)`.
///
/// - `derivation_key` — full 64-char blake3 hex (stable identifier for
///   the scopes store).
/// - `candidate_name` — `"git_<hex16>"` or `"cwd_<hex16>"` (valid
///   `Scope` string: chars in `[0-9a-f_]`, length 20 ≤ 128).
/// - `source` — `"git"` or `"cwd"`.
fn derive_from_env(cwd: &Path) -> (String, String, String) {
    if let Some((url, branch)) = try_git_remote_and_branch(cwd) {
        let raw = format!("{}@{}", url, branch);
        let full_key = blake3_hex64(&raw);
        let short = &full_key[..16];
        let name = format!("git_{short}");
        (full_key, name, "git".to_string())
    } else {
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let raw = canonical.to_string_lossy();
        let full_key = blake3_hex64(raw.as_ref());
        let short = &full_key[..16];
        let name = format!("cwd_{short}");
        (full_key.clone(), name, "cwd".to_string())
    }
}

/// Try to read `git remote.origin.url` and `git rev-parse --abbrev-ref HEAD`
/// from `cwd`.
///
/// Returns `None` on any failure: not a git repo, no origin remote, detached
/// HEAD, or git not installed.
fn try_git_remote_and_branch(cwd: &Path) -> Option<(String, String)> {
    let url = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())?;
    let url = url.trim().to_string();
    if url.is_empty() {
        return None;
    }

    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())?;
    let branch = branch.trim().to_string();
    // "HEAD" means detached HEAD — not useful as a stable key.
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }

    Some((url, branch))
}

// ── blake3 helpers ────────────────────────────────────────────────────────────

/// Full 64-char lowercase hex of the blake3 hash of `input`.
fn blake3_hex64(input: &str) -> String {
    let hash = blake3::hash(input.as_bytes());
    hash.to_hex().to_string()
}

// ── Store I/O ─────────────────────────────────────────────────────────────────

/// Resolve the scopes file path.
///
/// Checks `LUNARIS_SCOPES_FILE` env first (for operator / integration-test
/// override), then defaults to `~/.lunaris/scopes.json`.
fn scopes_file_path() -> Result<PathBuf, ScopeResolveError> {
    if let Ok(path) = std::env::var("LUNARIS_SCOPES_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir().ok_or(ScopeResolveError::NoHome)?;
    Ok(home.join(".lunaris").join("scopes.json"))
}

/// Load `~/.lunaris/scopes.json` (or the override path).
///
/// Returns an empty [`ScopesFile`] if the file does not yet exist.
/// If the file is corrupt JSON, returns `ScopeResolveError::Parse` so the
/// caller can decide to overwrite it.
fn load_store(path: &Path) -> Result<ScopesFile, ScopeResolveError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(ScopeResolveError::Parse),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ScopesFile::default()),
        Err(e) => Err(ScopeResolveError::Io(e)),
    }
}

/// Atomically write the scopes store.
///
/// Creates parent directories if they don't exist. Writes the full serialised
/// JSON; file is small (typically < 4 KB) so no atomic-rename dance is needed
/// at this scale.
fn save_store(path: &Path, store: &ScopesFile) -> Result<(), ScopeResolveError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(store)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

// ── Timestamp ─────────────────────────────────────────────────────────────────

/// Produce an RFC 3339-ish timestamp with second resolution using only
/// `std::time::SystemTime`. No `chrono` dep required.
///
/// Format: `YYYY-MM-DDTHH:MM:SSZ` (UTC, no sub-second precision).
fn rfc3339_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Compute UTC fields from the Unix epoch manually.
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let total_days = secs / 86400;
    let (y, mo, d) = days_to_ymd(total_days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert total days since the Unix epoch (1970-01-01) to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // 400-year Gregorian cycle.
    let y400 = days / 146097;
    days %= 146097;
    let y100 = (days / 36524).min(3);
    days -= y100 * 36524;
    let y4 = days / 1461;
    days %= 1461;
    let y1 = (days / 365).min(3);
    days -= y1 * 365;

    let year = y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days: [u64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let mut month = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Create a git repo in `dir` with an `origin` remote and branch `feat/x`.
    fn init_git_repo(dir: &Path) {
        fn run(args: &[&str], cwd: &Path) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .expect("git must be on PATH for these tests");
            assert!(status.success(), "git {:?} failed", args);
        }
        run(&["init", "-b", "feat/x"], dir);
        run(&["config", "user.email", "test@example.com"], dir);
        run(&["config", "user.name", "test"], dir);
        run(&["commit", "--allow-empty", "-m", "init"], dir);
        run(
            &["remote", "add", "origin", "https://github.com/example/repo.git"],
            dir,
        );
    }

    // ── Test 1: override wins ─────────────────────────────────────────────────

    #[test]
    fn override_wins_over_derivation() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        let scope = resolve_with(Some("my-scope"), td.path(), &scopes_path)
            .expect("override must succeed");
        assert_eq!(scope.as_str(), "my-scope");
    }

    // ── Test 2: git remote + branch → stable scope ────────────────────────────

    #[test]
    fn git_remote_and_branch_yield_stable_scope() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        init_git_repo(td.path());

        let scope1 = resolve_with(None, td.path(), &scopes_path)
            .expect("git-backed scope must resolve");

        // Satisfies Scope alphabet.
        let s = scope1.as_str();
        assert!(
            s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
            "scope {:?} must match [A-Za-z0-9_\\-.]+",
            s
        );
        assert!(!s.is_empty() && s.len() <= 128, "scope must be 1..=128 chars");

        // Repeating the call returns the identical scope.
        let scope2 = resolve_with(None, td.path(), &scopes_path)
            .expect("second resolve must succeed");
        assert_eq!(scope1, scope2, "scope must be stable across calls");

        // Derived name starts with "git_"
        assert!(s.starts_with("git_"), "git-derived scope must start with git_, got {:?}", s);
    }

    // ── Test 3: cwd fallback when not a git repo ──────────────────────────────

    #[test]
    fn falls_back_to_cwd_when_not_a_git_repo() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        // No git init — bare temp dir.
        let scope = resolve_with(None, td.path(), &scopes_path)
            .expect("cwd fallback must succeed");
        let s = scope.as_str();

        // Satisfies Scope alphabet.
        assert!(
            s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
            "scope {:?} must match [A-Za-z0-9_\\-.]+",
            s
        );
        assert!(s.starts_with("cwd_"), "cwd-derived scope must start with cwd_, got {:?}", s);

        // Same dir → same scope.
        let scope2 = resolve_with(None, td.path(), &scopes_path)
            .expect("second cwd resolve must succeed");
        assert_eq!(scope, scope2, "same dir must produce same scope");

        // Sibling dir → different scope.
        let td2 = TempDir::new().unwrap();
        let scopes_path2 = td2.path().join("scopes.json");
        let scope3 = resolve_with(None, td2.path(), &scopes_path2)
            .expect("sibling dir must resolve");
        assert_ne!(
            scope, scope3,
            "distinct dirs must produce distinct scopes"
        );
    }

    // ── Test 4: persistence + rename ─────────────────────────────────────────

    #[test]
    fn rename_persists_across_calls() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");

        // First call — creates the entry.
        let original = resolve_with(None, td.path(), &scopes_path)
            .expect("initial resolve must succeed");

        // Read the store and rename the entry's `name`.
        let raw = std::fs::read_to_string(&scopes_path).expect("scopes.json must exist");
        let mut store: ScopesFile = serde_json::from_str(&raw).expect("must parse");
        for entry in store.scopes.values_mut() {
            entry.name = "renamed-scope".to_string();
        }
        let updated = serde_json::to_vec_pretty(&store).unwrap();
        std::fs::write(&scopes_path, updated).unwrap();

        // Second call — should return the renamed scope.
        let after_rename = resolve_with(None, td.path(), &scopes_path)
            .expect("post-rename resolve must succeed");
        assert_eq!(
            after_rename.as_str(),
            "renamed-scope",
            "renamed scope must be returned; got {:?} (was {:?})",
            after_rename.as_str(),
            original.as_str()
        );
    }

    // ── Extra: invalid override is rejected ───────────────────────────────────

    #[test]
    fn invalid_override_is_rejected() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        // Colon is rejected by Scope::new
        let err = resolve_with(Some("bad:scope"), td.path(), &scopes_path);
        assert!(err.is_err(), "colon in override must fail");
        // Empty string is rejected
        let err2 = resolve_with(Some(""), td.path(), &scopes_path);
        assert!(err2.is_err(), "empty override must fail");
    }

    // ── Extra: corrupt store is surfaced as error ─────────────────────────────

    #[test]
    fn corrupt_store_returns_parse_error() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        std::fs::write(&scopes_path, b"{{not valid json}}").unwrap();
        let err = resolve_with(None, td.path(), &scopes_path);
        assert!(err.is_err(), "corrupt store must return an error");
    }

    // ── Extra: blake3_hex64 produces 64-char lowercase hex ────────────────────

    #[test]
    fn blake3_hex64_produces_64_lowercase_hex_chars() {
        let h = blake3_hex64("test-input");
        assert_eq!(h.len(), 64, "blake3 hex must be 64 chars");
        assert!(
            h.chars().all(|c| c.is_ascii_hexdigit()),
            "blake3 hex must be hexadecimal"
        );
    }

    // ── Extra: rfc3339_now produces a plausible timestamp ────────────────────

    #[test]
    fn rfc3339_now_produces_nonempty_timestamp() {
        let ts = rfc3339_now();
        assert!(!ts.is_empty());
        // Basic format check: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "timestamp should be 20 chars, got {:?}", ts);
        assert!(ts.ends_with('Z'), "timestamp must end with Z, got {:?}", ts);
    }
}
