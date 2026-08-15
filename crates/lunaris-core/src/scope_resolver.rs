//! Scope derivation — shared between `lunaris-mcp` and `lunaris-hook`.
//!
//! # Architecture
//!
//! This module owns the protocol (`ScopeStore` trait) and the pure derivation
//! algorithm (`resolve_with`). Concrete storage implementations live in the
//! consuming crates:
//!
//! - `lunaris-mcp`: `JsonScopesFileStore` wrapping `~/.lunaris/scopes.json`.
//! - `lunaris-hook`: its own parallel `JsonScopesFileStore` pointing at the
//!   same on-disk path (neither binary depends on the other).
//! - Tests: `InMemoryScopeStore` (no file I/O, defined here for convenience).
//!
//! # Scope precedence
//!
//! 1. Explicit override → `resolve_with` `override_` parameter.
//! 2. `git remote.origin.url + branch` → blake3 → `"git_<hex16>"`.
//! 3. Canonical cwd → blake3 → `"cwd_<hex16>"`.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{Scope, ScopeError};
use thiserror::Error;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors returned by [`resolve_with`].
#[derive(Debug, Error)]
pub enum ScopeResolveError {
    /// The `override_` value failed [`Scope::new`] validation.
    #[error("override scope failed validation: {0}")]
    InvalidOverride(#[source] ScopeError),

    /// The home directory could not be determined (missing `HOME` env on Unix
    /// or equivalent platform failure).
    #[error("could not resolve home directory")]
    NoHome,

    /// An I/O error while reading or writing the scopes store.
    #[error("scope store io: {0}")]
    Io(#[from] std::io::Error),

    /// The scopes store file exists but failed JSON parsing.
    #[error("scope store parse: {0}")]
    Parse(#[from] serde_json::Error),

    /// The blake3-derived scope name failed [`Scope::new`] validation. This
    /// should be impossible because derived names are `[git|cwd]_<16 hex
    /// chars>` which always satisfies the alphabet; this variant guards
    /// against future regressions.
    #[error("derived scope failed validation: {0}")]
    DerivationInvalid(#[source] ScopeError),

    /// No storage URL could be resolved for this scope.
    ///
    /// Through 0.6.x every resolver ended in a per-scope
    /// `sqlite:///<HOME>/.lunaris/<scope>.db` fallback, so this state was
    /// unreachable. 0.7.0 deleted the embedded backend, and with it the last
    /// substrate a resolver could conjure on its own — an unresolvable store is
    /// now a terminal condition rather than a silent downgrade.
    ///
    /// The payload is the caller's own operator-facing prose: `lunaris-hook`
    /// and `lunaris-mcp` have different exit ramps, and neither belongs in
    /// `lunaris-core`.
    #[error("{0}")]
    NoStoreUrl(String),
}

// ── Shared data model ─────────────────────────────────────────────────────────

/// One entry in the scope registry.
///
/// Used by both `lunaris-mcp` and `lunaris-hook` as the in-memory
/// representation when reading/writing the scope store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScopeRecord {
    /// The [`Scope`] string for this derivation key.
    /// Users may rename this field manually; `resolve_with` honours it.
    pub name: String,
    /// RFC 3339-ish timestamp of first derivation (seconds resolution).
    pub created_at: String,
    /// How the scope was derived: `"git"` or `"cwd"`.
    pub source: String,
}

// ── ScopeStore trait ──────────────────────────────────────────────────────────

/// Backend-agnostic protocol for reading and writing the scope registry.
///
/// The registry maps a full blake3 hex key → [`ScopeRecord`].
///
/// Concrete implementations:
/// - [`InMemoryScopeStore`] — for tests (no file I/O).
/// - `JsonScopesFileStore` in `lunaris-mcp` — reads/writes `~/.lunaris/scopes.json`.
/// - `JsonScopesFileStore` in `lunaris-hook` — parallel impl, same on-disk path.
pub trait ScopeStore {
    /// Read all scope registry entries, keyed by the full blake3 hex.
    fn read(&self) -> Result<BTreeMap<String, ScopeRecord>, ScopeResolveError>;
    /// Persist the full scope registry, replacing any prior contents.
    fn write(&self, scopes: &BTreeMap<String, ScopeRecord>) -> Result<(), ScopeResolveError>;
}

// ── InMemoryScopeStore ────────────────────────────────────────────────────────

/// In-memory scope store for tests. Persists across multiple calls within
/// the same process (which is required for `resolve_with_git_stable` to pass:
/// the second call must read back the entry written by the first call).
///
/// Not `Send` or `Sync` due to `RefCell`; suitable for single-threaded tests.
pub struct InMemoryScopeStore {
    inner: RefCell<BTreeMap<String, ScopeRecord>>,
}

impl InMemoryScopeStore {
    /// Create a new empty in-memory store.
    pub fn new() -> Self {
        Self { inner: RefCell::new(BTreeMap::new()) }
    }
}

impl Default for InMemoryScopeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeStore for InMemoryScopeStore {
    fn read(&self) -> Result<BTreeMap<String, ScopeRecord>, ScopeResolveError> {
        Ok(self.inner.borrow().clone())
    }

    fn write(&self, scopes: &BTreeMap<String, ScopeRecord>) -> Result<(), ScopeResolveError> {
        *self.inner.borrow_mut() = scopes.clone();
        Ok(())
    }
}

// ── Core algorithm ────────────────────────────────────────────────────────────

/// Derive or restore the [`Scope`] for the current session.
///
/// Precedence:
/// 1. `override_` if `Some` — validated via [`Scope::new`], never persisted.
/// 2. `git remote.origin.url` + active branch → blake3 → `"git_<hex16>"`.
/// 3. Canonical `cwd` → blake3 → `"cwd_<hex16>"`.
///
/// Derived scopes are persisted to the `store` (keyed by the full blake3 hex).
/// If the key is already present, the stored `name` is returned — allowing
/// users to rename a scope and have the rename survive across sessions.
///
/// # Parameter order
///
/// `(cwd, store, override_)` — the store is the generic dispatch point, so it
/// sits between the static input (cwd) and the optional override.
pub fn resolve_with<S: ScopeStore>(
    cwd: &Path,
    store: &S,
    override_: Option<&str>,
) -> Result<Scope, ScopeResolveError> {
    // 1 — explicit override (highest priority, never persisted)
    if let Some(o) = override_ {
        return Scope::new(o).map_err(ScopeResolveError::InvalidOverride);
    }

    // 2 / 3 — derive from git or cwd
    let (derivation_key, candidate_name, source) = derive_from_env(cwd);

    // 4 — load store, honour any persisted rename, persist new entry if needed
    let mut scopes = store.read()?;

    let scope_str = if let Some(entry) = scopes.get(&derivation_key) {
        entry.name.clone()
    } else {
        let record =
            ScopeRecord { name: candidate_name.clone(), created_at: rfc3339_now(), source };
        scopes.insert(derivation_key.clone(), record);
        store.write(&scopes)?;
        candidate_name
    };

    Scope::new(&scope_str).map_err(ScopeResolveError::DerivationInvalid)
}

// ── Path helper ───────────────────────────────────────────────────────────────

/// Resolve the scopes file path.
///
/// Checks `LUNARIS_SCOPES_FILE` env first (for operator / integration-test
/// override), then defaults to `~/.lunaris/scopes.json`.
///
/// Exposed as `pub` so both `lunaris-mcp` and `lunaris-hook` can resolve the
/// same default path without duplicating the logic.
pub fn scopes_file_path() -> Result<PathBuf, ScopeResolveError> {
    if let Ok(path) = std::env::var("LUNARIS_SCOPES_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir().ok_or(ScopeResolveError::NoHome)?;
    Ok(home.join(".lunaris").join("scopes.json"))
}

// ── blake3 helper ─────────────────────────────────────────────────────────────

/// Full 64-char lowercase hex of the blake3 hash of `input`.
pub fn blake3_hex64(input: &str) -> String {
    let hash = blake3::hash(input.as_bytes());
    hash.to_hex().to_string()
}

// ── Private derivation helpers ────────────────────────────────────────────────

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

// ── Timestamp ─────────────────────────────────────────────────────────────────

/// Produce an RFC 3339-ish timestamp with second resolution using only
/// `std::time::SystemTime`. No `chrono` dep required.
///
/// Format: `YYYY-MM-DDTHH:MM:SSZ` (UTC, no sub-second precision).
fn rfc3339_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
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
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let month_days: [u64; 12] =
        [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

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
