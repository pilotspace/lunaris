//! Scope resolution for `lunaris-hook`.
//!
//! # Design (B3 + W5 fixes)
//!
//! Delegates to `lunaris_core::scope_resolver::resolve_with<S: ScopeStore>`
//! (shared with `lunaris-mcp`). Both binaries produce bit-identical scope names
//! for the same repo.
//!
//! `lunaris-hook` has its OWN `JsonScopesFileStore` impl — it does not import
//! the one in `lunaris-mcp`. Both point at the same `~/.lunaris/scopes.json`
//! on disk. Neither binary depends on the other.
//!
//! # Storage URL default (W5 fix)
//!
//! Default is `sqlite://~/.lunaris/<scope>.db` — mirrors `lunaris-mcp`'s
//! `resolve_storage_url` in `state.rs` (per-scope partition). ROADMAP SC#1
//! prose says "memory.db" but the canonical pattern from state.rs is
//! `<scope>.db`. Both binaries derive identical scopes → they naturally write
//! to the same file. Override via `LUNARIS_STORE_URL` (W7 fix — no
//! `LUNARIS_HOOK_STORAGE` alias; single shared env var across both binaries).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lunaris_core::Scope;
use lunaris_core::scope_resolver::{
    ScopeRecord, ScopeResolveError, ScopeStore, resolve_with, scopes_file_path,
};

// ── JsonScopesFileStore (lunaris-hook's own impl) ─────────────────────────────
//
// Parallel to lunaris-mcp's JsonScopesFileStore. Same JSON format, same
// on-disk path. Neither binary imports the other's concrete type.

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct ScopesFile {
    scopes: BTreeMap<String, ScopeEntry>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone)]
struct ScopeEntry {
    name: String,
    created_at: String,
    source: String,
}

fn load_store_from(path: &Path) -> Result<ScopesFile, ScopeResolveError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ScopesFile::default()),
        Err(e) => Err(ScopeResolveError::Io(e)),
    }
}

fn save_store_to(path: &Path, store: &ScopesFile) -> Result<(), ScopeResolveError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(store)?;
    Ok(std::fs::write(path, bytes)?)
}

/// File-backed scope store for `lunaris-hook`.
///
/// Implements `ScopeStore` by reading/writing `~/.lunaris/scopes.json`.
/// Same JSON format as `lunaris-mcp`'s `JsonScopesFileStore`; both binaries
/// share the file on disk, neither imports the other.
pub(crate) struct JsonScopesFileStore {
    pub path: PathBuf,
}

impl ScopeStore for JsonScopesFileStore {
    fn read(&self) -> Result<BTreeMap<String, ScopeRecord>, ScopeResolveError> {
        let store = load_store_from(&self.path)?;
        Ok(store.scopes.into_iter().map(|(k, e)| (k, ScopeRecord {
            name: e.name,
            created_at: e.created_at,
            source: e.source,
        })).collect())
    }

    fn write(&self, scopes: &BTreeMap<String, ScopeRecord>) -> Result<(), ScopeResolveError> {
        let file = ScopesFile {
            scopes: scopes.iter().map(|(k, r)| (k.clone(), ScopeEntry {
                name: r.name.clone(),
                created_at: r.created_at.clone(),
                source: r.source.clone(),
            })).collect(),
        };
        save_store_to(&self.path, &file)
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Resolve the scope for this hook invocation.
///
/// Precedence:
/// 1. `LUNARIS_HOOK_SCOPE` env var (override).
/// 2. `cwd + git remote.origin.url + branch` → blake3 → "git_<hex16>".
/// 3. Canonical cwd → blake3 → "cwd_<hex16>".
///
/// Writes to `~/.lunaris/scopes.json` (or `LUNARIS_SCOPES_FILE` override).
pub fn resolve(cwd: &Path) -> Result<Scope, ScopeResolveError> {
    let override_ = std::env::var("LUNARIS_HOOK_SCOPE").ok();
    let scopes_path = if let Ok(p) = std::env::var("LUNARIS_SCOPES_FILE") {
        PathBuf::from(p)
    } else {
        scopes_file_path()?
    };
    let store = JsonScopesFileStore { path: scopes_path };
    resolve_with(cwd, &store, override_.as_deref())
}

/// Testable variant — accepts explicit scopes_path + override for test isolation.
pub fn resolve_with_path(
    cwd: &Path,
    scopes_path: &Path,
    override_: Option<&str>,
) -> Result<Scope, ScopeResolveError> {
    let store = JsonScopesFileStore { path: scopes_path.to_path_buf() };
    resolve_with(cwd, &store, override_)
}

/// Derive the SQLite storage URL for the given scope.
///
/// Priority (W5 + W7 fix):
/// 1. `LUNARIS_STORE_URL` env var — shared with lunaris-mcp naming convention,
///    passed verbatim. Replaces the former LUNARIS_HOOK_STORAGE alias.
/// 2. `sqlite:///<HOME>/.lunaris/<scope>.db` — per-scope partition, mirrors
///    `lunaris-mcp/src/state.rs::resolve_storage_url` exactly.
///
/// Both binaries derive identical scopes for the same repo, so they naturally
/// read/write the same `<scope>.db` file without any extra coordination.
pub fn resolve_storage_url(scope: &Scope) -> Result<String, ScopeResolveError> {
    if let Ok(url) = std::env::var("LUNARIS_STORE_URL") {
        return Ok(url);
    }
    let home = dirs::home_dir().ok_or(ScopeResolveError::NoHome)?;
    let dir = home.join(".lunaris");
    std::fs::create_dir_all(&dir).map_err(ScopeResolveError::Io)?;
    Ok(format!("sqlite://{}", dir.join(format!("{}.db", scope.as_str())).display()))
}
