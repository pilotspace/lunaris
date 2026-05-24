//! `memory.list_scopes` — enumerate known memory scopes.
//!
//! Reads `~/.lunaris/scopes.json` (the same registry [`crate::scope_resolver`]
//! maintains) and returns every persisted scope entry sorted by `created_at`
//! ascending for stable, deterministic output.
//!
//! ## Design invariants
//!
//! - **NEVER scans `~/.lunaris/*.db` files.** The JSON registry is the
//!   canonical source of truth for scope membership per RFC 0001. Scanning
//!   `.db` files would be a cross-scope read — a type error by design.
//! - Missing `scopes.json` → empty list (not an error; fresh install).
//! - Corrupt `scopes.json` → `ToolError::InvalidInput` with the parse error.
//! - All scopes in the JSON are returned regardless of the bound scope
//!   (the user owns the file; stdio transport is process-local, not multi-tenant).

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::scope_resolver::{ScopeResolveError, load_scopes_from, scopes_file_path};
use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.list_scopes`.
///
/// Currently empty — included as a struct so the MCP schema is stable
/// even if parameters are added in a later wave.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListScopesParams {}

/// Metadata for one memory scope.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScopeEntry {
    /// Canonical scope name (validated `[A-Za-z0-9_\-.]{1,128}`).
    pub name: String,
    /// ISO-8601 UTC creation timestamp (seconds resolution, `Z` suffix).
    pub created_at: String,
    /// How the scope was derived: `"git"`, `"cwd"`, or `"override"`.
    pub source: String,
}

/// Output of a successful `memory.list_scopes` call.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ListScopesResponse {
    /// All known scopes, ordered by `created_at` ascending (oldest first).
    pub scopes: Vec<ScopeEntry>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.list_scopes`.
///
/// Reads `~/.lunaris/scopes.json` (or `LUNARIS_SCOPES_FILE` env override) and
/// returns all persisted entries sorted by `created_at` ascending.
///
/// Never scans `.db` files — the JSON registry is the only source of truth.
pub(crate) async fn handle(
    _state: &AppState,
    _params: ListScopesParams,
) -> Result<ListScopesResponse, ToolError> {
    let path = scopes_file_path().map_err(map_resolve_error)?;
    handle_with(&path)
}

/// Testable variant — accepts an explicit `scopes_path` so tests can use a
/// `TempDir` without touching `~/.lunaris/scopes.json` or relying on
/// `LUNARIS_SCOPES_FILE` (which is process-global and fragile under
/// `cargo test`'s default parallelism).
pub(crate) fn handle_with(scopes_path: &Path) -> Result<ListScopesResponse, ToolError> {
    let records = load_scopes_from(scopes_path).map_err(map_resolve_error)?;

    let mut entries: Vec<ScopeEntry> = records
        .into_iter()
        .map(|r| ScopeEntry { name: r.name, created_at: r.created_at, source: r.source })
        .collect();

    // Sort by created_at ascending (lexicographic on ISO-8601 strings with
    // the same `YYYY-MM-DDTHH:MM:SSZ` format is correct because the format
    // is zero-padded and UTC-terminated — lex order == chronological order).
    entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    Ok(ListScopesResponse { scopes: entries })
}

// ── Error mapping ─────────────────────────────────────────────────────────────

/// Map a [`ScopeResolveError`] to a [`ToolError`].
///
/// - `NoHome` / `Io` → `InvalidInput` (operator-visible configuration issue).
/// - `Parse` → `InvalidInput` with the JSON parse error (corrupt file).
/// - Other variants should not arise in the `list_scopes` code path but are
///   mapped to `InvalidInput` for completeness.
fn map_resolve_error(e: ScopeResolveError) -> ToolError {
    ToolError::InvalidInput(format!("scopes.json error: {e}"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // All tests use handle_with() directly — no AppState needed since
    // list_scopes never touches the Lunaris engine, only scopes.json.

    // ── Test 1: fresh install (no scopes.json) ────────────────────────────────

    #[test]
    fn returns_empty_on_fresh_install() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        // File does NOT exist — simulates fresh install.
        let resp = handle_with(&scopes_path).expect("missing file must return empty, not error");
        assert!(
            resp.scopes.is_empty(),
            "fresh install must return empty scopes, got {:?}",
            resp.scopes
        );
    }

    // ── Test 2: entries sorted by created_at ascending ────────────────────────

    #[test]
    fn returns_all_scopes_sorted_by_created_at() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");

        // Pre-populate with 3 entries at t1 < t2 < t3 (different timestamps).
        let store = serde_json::json!({
            "scopes": {
                "aaaa": {
                    "name": "scope-b",
                    "created_at": "2024-06-01T10:00:00Z",
                    "source": "cwd"
                },
                "bbbb": {
                    "name": "scope-a",
                    "created_at": "2024-01-01T00:00:00Z",
                    "source": "git"
                },
                "cccc": {
                    "name": "scope-c",
                    "created_at": "2025-01-01T12:00:00Z",
                    "source": "cwd"
                }
            }
        });
        std::fs::write(&scopes_path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let resp = handle_with(&scopes_path).expect("valid scopes.json must parse");
        assert_eq!(resp.scopes.len(), 3);
        // Verify ascending created_at order.
        assert_eq!(resp.scopes[0].name, "scope-a", "oldest entry first");
        assert_eq!(resp.scopes[1].name, "scope-b");
        assert_eq!(resp.scopes[2].name, "scope-c", "newest entry last");
    }

    // ── Test 3: no *.db scanning ──────────────────────────────────────────────

    #[test]
    fn does_not_scan_sqlite_files() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");

        // Populate scopes.json with exactly 1 entry.
        let store = serde_json::json!({
            "scopes": {
                "deadbeef": {
                    "name": "real-scope",
                    "created_at": "2024-01-01T00:00:00Z",
                    "source": "cwd"
                }
            }
        });
        std::fs::write(&scopes_path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        // Create 3 spurious *.db files alongside scopes.json — they MUST NOT
        // be picked up as additional scopes.
        std::fs::write(td.path().join("ghost-scope-1.db"), b"").unwrap();
        std::fs::write(td.path().join("ghost-scope-2.db"), b"").unwrap();
        std::fs::write(td.path().join("ghost-scope-3.db"), b"").unwrap();

        let resp = handle_with(&scopes_path).expect("must succeed");
        assert_eq!(
            resp.scopes.len(),
            1,
            "only the JSON-registered scope should appear; got {:?}",
            resp.scopes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(resp.scopes[0].name, "real-scope");
    }

    // ── Test 4: corrupt scopes.json surfaces parse error ─────────────────────

    #[test]
    fn corrupt_scopes_file_surfaces_parse_error() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");
        std::fs::write(&scopes_path, b"not valid json {{").unwrap();

        let err = handle_with(&scopes_path).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "corrupt file must return InvalidInput, got {err:?}"
        );
        if let ToolError::InvalidInput(msg) = err {
            assert!(
                msg.contains("scopes.json") || msg.contains("parse") || msg.contains("Parse"),
                "error message should mention scopes.json or parse: {msg}"
            );
        }
    }

    // ── Test 5: single entry round-trips all fields correctly ─────────────────

    #[test]
    fn single_entry_fields_are_preserved() {
        let td = TempDir::new().unwrap();
        let scopes_path = td.path().join("scopes.json");

        let store = serde_json::json!({
            "scopes": {
                "cafebabe": {
                    "name": "my-project",
                    "created_at": "2025-05-24T09:00:00Z",
                    "source": "git"
                }
            }
        });
        std::fs::write(&scopes_path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let resp = handle_with(&scopes_path).expect("must succeed");
        assert_eq!(resp.scopes.len(), 1);
        let entry = &resp.scopes[0];
        assert_eq!(entry.name, "my-project");
        assert_eq!(entry.created_at, "2025-05-24T09:00:00Z");
        assert_eq!(entry.source, "git");
    }
}
