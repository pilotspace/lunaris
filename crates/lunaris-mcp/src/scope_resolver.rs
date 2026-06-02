//! Scope resolver for `lunaris-mcp`.
//!
//! The protocol (`ScopeStore` trait) and pure derivation live in
//! `lunaris-core::scope_resolver`. This module provides:
//!
//! 1. `JsonScopesFileStore` — the concrete file-backed store for `lunaris-mcp`,
//!    implementing `ScopeStore` by wrapping the existing `ScopesFile` JSON format.
//! 2. Thin `resolve` / `resolve_with_path` wrappers that construct a
//!    `JsonScopesFileStore` and forward to the generic `lunaris_core::scope_resolver::resolve_with`.
//! 3. `ScopeRecord` + `load_scopes_from` — MCP-only types consumed by `list_scopes::handle`.
//!
//! Both `lunaris-mcp` and `lunaris-hook` have their own `JsonScopesFileStore` impl
//! pointing at the same `~/.lunaris/scopes.json`. Neither binary depends on the
//! other; they share the on-disk file by convention.

pub(crate) use lunaris_core::scope_resolver::{
    ScopeRecord, ScopeResolveError, ScopeStore, scopes_file_path,
};

use lunaris_core::Scope;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

// ── Internal ScopesFile JSON format (owned by lunaris-mcp) ───────────────────
//
// `ScopesFile` / `ScopeEntry` / `load_store` / `save_store` stay here per
// CONTEXT.md §"Scope resolver sharing": "persistence stays in lunaris-mcp".
// `JsonScopesFileStore` wraps them to implement the `ScopeStore` protocol.

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

fn load_store(path: &Path) -> Result<ScopesFile, ScopeResolveError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ScopesFile::default()),
        Err(e) => Err(ScopeResolveError::Io(e)),
    }
}

fn save_store(path: &Path, store: &ScopesFile) -> Result<(), ScopeResolveError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(store)?;
    Ok(std::fs::write(path, bytes)?)
}

// ── JsonScopesFileStore (implements ScopeStore) ───────────────────────────────

/// File-backed scope store for `lunaris-mcp`.
///
/// Implements [`ScopeStore`] by reading/writing `~/.lunaris/scopes.json`
/// (or the path provided by the caller). The JSON format is owned here;
/// `lunaris-hook` has a parallel implementation pointing at the same path.
pub(crate) struct JsonScopesFileStore {
    pub path: PathBuf,
}

impl ScopeStore for JsonScopesFileStore {
    fn read(&self) -> Result<BTreeMap<String, ScopeRecord>, ScopeResolveError> {
        let store = load_store(&self.path)?;
        Ok(store
            .scopes
            .into_iter()
            .map(|(k, e)| {
                (k, ScopeRecord { name: e.name, created_at: e.created_at, source: e.source })
            })
            .collect())
    }

    fn write(&self, scopes: &BTreeMap<String, ScopeRecord>) -> Result<(), ScopeResolveError> {
        let file = ScopesFile {
            scopes: scopes
                .iter()
                .map(|(k, r)| {
                    (
                        k.clone(),
                        ScopeEntry {
                            name: r.name.clone(),
                            created_at: r.created_at.clone(),
                            source: r.source.clone(),
                        },
                    )
                })
                .collect(),
        };
        save_store(&self.path, &file)
    }
}

// ── Public API (wraps the generic resolve_with from lunaris-core) ─────────────

/// Resolve the scope for the current process using the default scopes file.
pub(crate) fn resolve(override_: Option<&str>) -> Result<Scope, ScopeResolveError> {
    let cwd = std::env::current_dir().map_err(ScopeResolveError::Io)?;
    let path = scopes_file_path()?;
    let store = JsonScopesFileStore { path };
    lunaris_core::scope_resolver::resolve_with(&cwd, &store, override_)
}

/// Testable variant — accepts explicit cwd + scopes_path for test isolation.
#[cfg(test)]
pub(crate) fn resolve_with_path(
    override_: Option<&str>,
    cwd: &Path,
    scopes_path: &Path,
) -> Result<Scope, ScopeResolveError> {
    let store = JsonScopesFileStore { path: scopes_path.to_path_buf() };
    lunaris_core::scope_resolver::resolve_with(cwd, &store, override_)
}

// ── MCP-only: ScopeRecord list for list_scopes::handle ───────────────────────
//
// `load_scopes_from` is a read-only helper for the `list_scopes` MCP tool.
// It is NOT part of the ScopeStore protocol — it returns a Vec<ScopeRecord>
// for display, not a BTreeMap for mutation.

/// Load and return all scope entries from `path` as a flat `Vec<ScopeRecord>`.
///
/// - Missing file → empty `Vec` (fresh install, not an error).
/// - Corrupt JSON → `ScopeResolveError::Parse`.
pub(crate) fn load_scopes_from(path: &Path) -> Result<Vec<ScopeRecord>, ScopeResolveError> {
    let store = load_store(path)?;
    Ok(store
        .scopes
        .into_values()
        .map(|e| ScopeRecord { name: e.name, created_at: e.created_at, source: e.source })
        .collect())
}
