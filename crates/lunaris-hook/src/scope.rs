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

/// Resolve the scope for a **daemon request**. Unlike [`resolve`], this ignores
/// the process's own `LUNARIS_HOOK_SCOPE` env var.
///
/// A long-lived `contextd` inherits `LUNARIS_HOOK_SCOPE` at *birth*; honoring it
/// for request handling stamps that one scope onto every project's unpinned
/// request — the P0 cross-project scope bleed observed 2026-07-14 (a daemon born
/// under `cc-hook-e2e` swallowed captures from every repo). The caller
/// (`context::resolve_scope`) applies any *explicit* request scope BEFORE this;
/// here we derive purely from `cwd`. `LUNARIS_SCOPES_FILE` is still honored — it
/// is a storage location, not a scope-identity override.
pub fn resolve_no_env(cwd: &Path) -> Result<Scope, ScopeResolveError> {
    let scopes_path = if let Ok(p) = std::env::var("LUNARIS_SCOPES_FILE") {
        PathBuf::from(p)
    } else {
        scopes_file_path()?
    };
    let store = JsonScopesFileStore { path: scopes_path };
    resolve_with(cwd, &store, None)
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

/// Derive the storage URL for the given scope.
///
/// Priority (W5 + W7 fix + contextd embedded-moon unification):
/// 1. `LUNARIS_STORE_URL` env var — shared with lunaris-mcp naming convention,
///    passed verbatim. Replaces the former LUNARIS_HOOK_STORAGE alias.
/// 2. contextd's embedded Moon, discovered via `~/.lunaris/contextd-moon.url`
///    and liveness-probed (see [`discover_contextd_moon`]). This is what keeps
///    the ONE-SHOT hook binary and the contextd daemon writing to the SAME
///    store when contextd bundles Moon in-process — without it the hook's
///    direct open would split-brain into per-scope SQLite while contextd
///    captures land in Moon.
/// 3. `sqlite:///<HOME>/.lunaris/<scope>.db` — per-scope partition, mirrors
///    `lunaris-mcp/src/state.rs::resolve_storage_url` exactly.
///
/// Both binaries derive identical scopes for the same repo AND resolve through
/// this same function, so they naturally converge on one store per priority
/// level without extra coordination.
pub fn resolve_storage_url(scope: &Scope) -> Result<String, ScopeResolveError> {
    if let Ok(url) = std::env::var("LUNARIS_STORE_URL") {
        return Ok(url);
    }
    let home = dirs::home_dir().ok_or(ScopeResolveError::NoHome)?;
    let lunaris_dir = home.join(".lunaris");
    resolve_storage_url_at(scope, &lunaris_dir)
}

/// Testable body of [`resolve_storage_url`] — takes the `~/.lunaris` dir
/// explicitly so tests can point it at a tempdir (no env mutation: `set_var`
/// is `unsafe fn` in edition 2024 and this crate forbids unsafe). The env
/// override stays in the public wrapper.
pub fn resolve_storage_url_at(
    scope: &Scope,
    lunaris_dir: &Path,
) -> Result<String, ScopeResolveError> {
    if let Some(url) = discover_contextd_moon(&lunaris_dir.join(CONTEXTD_MOON_URL_FILE)) {
        return Ok(url);
    }
    std::fs::create_dir_all(lunaris_dir).map_err(ScopeResolveError::Io)?;
    Ok(format!("sqlite://{}", lunaris_dir.join(format!("{}.db", scope.as_str())).display()))
}

/// Discovery-file name under `~/.lunaris`. Written by `lunaris-contextd` when
/// its embedded Moon is ready (feature `embedded-moon`); read by EVERY
/// resolver, feature or not — a missing file simply falls through.
pub const CONTEXTD_MOON_URL_FILE: &str = "contextd-moon.url";

/// Read the contextd embedded-Moon discovery file and liveness-probe the
/// advertised endpoint. Returns `Some("moon://…")` only when the endpoint
/// accepts a TCP connect within the probe budget (default 25ms, override
/// `LUNARIS_MOON_DISCOVERY_TIMEOUT_MS`); a stale file left by a dead contextd
/// fails the probe and falls through to SQLite — the hook must NEVER hang or
/// error on somebody else's leftovers (fail-open, matches the hook's
/// fail-open contract everywhere else).
///
/// The probe proves TCP accept, not RESP — the full PING readiness check
/// belongs to the launcher at startup. A foreign process squatting the exact
/// ephemeral port contextd advertised AND contextd being dead is the residual
/// window; `Lunaris::open` fails loudly in that case rather than corrupting.
fn discover_contextd_moon(url_file: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(url_file).ok()?;
    let url = raw.trim();
    let addr = url.strip_prefix("moon://")?;
    let sock: std::net::SocketAddr = addr.parse().ok()?;
    let timeout_ms = std::env::var("LUNARIS_MOON_DISCOVERY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(25);
    let timeout = std::time::Duration::from_millis(timeout_ms);
    std::net::TcpStream::connect_timeout(&sock, timeout).ok()?;
    Some(url.to_string())
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    fn scope() -> Scope {
        Scope::new("test-discovery-scope").unwrap()
    }

    #[test]
    fn no_discovery_file_falls_through_to_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        let url = resolve_storage_url_at(&scope(), tmp.path()).unwrap();
        assert!(
            url.starts_with("sqlite://") && url.ends_with("test-discovery-scope.db"),
            "absent discovery file must yield per-scope sqlite, got: {url}"
        );
    }

    #[test]
    fn live_discovery_endpoint_wins_over_sqlite() {
        // A real listener stands in for contextd's embedded Moon: the probe
        // only checks TCP accept.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONTEXTD_MOON_URL_FILE),
            format!("moon://127.0.0.1:{port}\n"),
        )
        .unwrap();
        let url = resolve_storage_url_at(&scope(), tmp.path()).unwrap();
        assert_eq!(
            url,
            format!("moon://127.0.0.1:{port}"),
            "live discovery endpoint must be preferred over sqlite"
        );
    }

    #[test]
    fn stale_discovery_file_falls_through_to_sqlite() {
        // Bind then DROP the listener — the advertised port is dead, exactly
        // like a discovery file left behind by a crashed contextd.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONTEXTD_MOON_URL_FILE),
            format!("moon://127.0.0.1:{port}\n"),
        )
        .unwrap();
        let url = resolve_storage_url_at(&scope(), tmp.path()).unwrap();
        assert!(
            url.starts_with("sqlite://"),
            "dead discovery endpoint must fall through to sqlite, got: {url}"
        );
    }

    #[test]
    fn garbage_discovery_file_falls_through_to_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(CONTEXTD_MOON_URL_FILE), "redis://not-a-moon-url:abc\n")
            .unwrap();
        let url = resolve_storage_url_at(&scope(), tmp.path()).unwrap();
        assert!(
            url.starts_with("sqlite://"),
            "unparseable discovery file must fall through to sqlite, got: {url}"
        );
    }
}
