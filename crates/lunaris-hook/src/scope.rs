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
//! # Storage URL resolution (W5 fix; no-default since 0.7.0)
//!
//! Two sources, in order: `LUNARIS_STORE_URL` (W7 fix — no
//! `LUNARIS_HOOK_STORAGE` alias; single shared env var across both binaries),
//! then a live `lunaris-contextd` embedded Moon discovered via
//! `~/.lunaris/contextd-moon.url`. The discovery half now lives in
//! `lunaris_core::store_discovery` — `lunaris-mcp` resolves through the same
//! function (task #28), so "the advertised store" means one thing repo-wide.
//!
//! There is **no third step**. Through 0.6.x this function ended in
//! `sqlite://~/.lunaris/<scope>.db`; 0.7.0 deleted the embedded backend, so
//! that line would now mint a URL `lunaris::open` refuses. Rather than defer
//! the failure to a scheme error two frames later — or invent a
//! `moon://localhost:6380` default, which risks writing an agent's captures
//! into whatever unrelated Moon happens to own that port — an unresolvable
//! store is a named error carrying the external-Moon quickstart.

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
/// 2. `cwd + git remote.origin.url + branch` → blake3 → "`git_<hex16>`".
/// 3. Canonical cwd → blake3 → "`cwd_<hex16>`".
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

/// The operator exit ramp printed when no storage URL can be resolved.
///
/// One constant so the env-var path, the discovery path, and any future caller
/// cannot drift into telling an operator three different stories.
pub const NO_STORE_URL_HELP: &str = "\
no Lunaris store is reachable: `LUNARIS_STORE_URL` is unset and no live \
lunaris-contextd Moon is advertised at `~/.lunaris/contextd-moon.url`. \
0.7.0 is Moon-only — the per-scope `sqlite:///<HOME>/.lunaris/<scope>.db` \
fallback was deleted with the embedded backend, and there is deliberately no \
replacement default (guessing a port could route an agent's captures into an \
unrelated Moon). Stand one up:\n  \
curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | sh\n  \
moon --bind 127.0.0.1 --port 6380 --shards 1 --dir ~/.lunaris/moon\n\
then export LUNARIS_STORE_URL=moon://127.0.0.1:6380 . Moon MUST run with \
`--shards 1` (Lunaris ingest is a single-shard TXN). Full recipe — durability, \
health checks, container flags: docs/operations/external-moon.md.";

/// Derive the storage URL for the given scope.
///
/// Priority (W5 + W7 fix + contextd embedded-moon unification):
/// 1. `LUNARIS_STORE_URL` env var — shared with lunaris-mcp naming convention,
///    passed verbatim. Replaces the former LUNARIS_HOOK_STORAGE alias.
/// 2. contextd's embedded Moon, discovered via `~/.lunaris/contextd-moon.url`
///    and liveness-probed (see `discover_contextd_moon`). This is what keeps
///    the ONE-SHOT hook binary and the contextd daemon writing to the SAME
///    store when contextd bundles Moon in-process.
///
/// Anything else is [`ScopeResolveError::NoStoreUrl`] carrying
/// [`NO_STORE_URL_HELP`]. Step 3 used to be a per-scope SQLite file; 0.7.0
/// deleted that backend (see the module docs).
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
///
/// `scope` is no longer read: it only ever named the per-scope SQLite file.
/// The parameter is kept so every call site and the discovery contract stay
/// put, and so re-introducing a scope-partitioned store later is a one-line
/// change rather than a signature break.
pub fn resolve_storage_url_at(
    scope: &Scope,
    lunaris_dir: &Path,
) -> Result<String, ScopeResolveError> {
    let _ = scope;
    if let Some(url) = discover_contextd_moon(lunaris_dir).into_url() {
        return Ok(url);
    }
    Err(ScopeResolveError::NoStoreUrl(NO_STORE_URL_HELP.to_string()))
}

/// Discovery-file name under `~/.lunaris`. Written by `lunaris-contextd` when
/// its embedded Moon is ready (feature `embedded-moon`); read by EVERY
/// resolver, feature or not — a missing file simply falls through.
///
/// Re-exported from `lunaris_core::store_discovery` so `contextd.rs` (the
/// WRITER) and every reader name the same constant. Since task #28 the probe
/// itself lives there too, shared with `lunaris-mcp`; neither binary depends
/// on the other.
pub use lunaris_core::store_discovery::{CONTEXTD_MOON_URL_FILE, discover_contextd_moon};

#[cfg(test)]
mod discovery_tests {
    //! Hook-side precedence. The probe's own semantics (stale port, non-loopback,
    //! TCP-without-PONG, garbage) are pinned once in
    //! `lunaris_core::store_discovery` — duplicating them here would only mean
    //! two places to update when the contract moves.

    use super::*;

    fn scope() -> Scope {
        Scope::new("test-discovery-scope").unwrap()
    }

    /// Assert the no-store outcome: a NAMED error whose text an operator can
    /// act on without reading the source.
    ///
    /// Each of these cases used to assert `url.starts_with("sqlite://")`. The
    /// claim being made was never "SQLite" — it was "this discovery input is
    /// not trusted". With the fallback gone, the same claim is that the
    /// resolver refuses, and refuses *legibly*: naming the env var to set, the
    /// `--shards 1` requirement Moon ingest depends on, and the runbook.
    #[track_caller]
    fn assert_refused_with_exit_ramp(result: Result<String, ScopeResolveError>) {
        let Err(err) = result else {
            panic!("untrusted/absent discovery must not resolve a store URL, got: {result:?}");
        };
        assert!(
            matches!(err, ScopeResolveError::NoStoreUrl(_)),
            "must be the named NoStoreUrl variant, got: {err:?}"
        );
        let msg = err.to_string();
        for needle in
            ["LUNARIS_STORE_URL", "moon://", "--shards 1", "docs/operations/external-moon.md"]
        {
            assert!(msg.contains(needle), "exit ramp must mention {needle}: {msg}");
        }
    }

    #[test]
    fn no_discovery_file_is_a_named_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        assert_refused_with_exit_ramp(resolve_storage_url_at(&scope(), tmp.path()));
    }

    /// Minimal RESP responder standing in for contextd's embedded Moon:
    /// accepts one connection and answers `+PONG` to anything.
    fn spawn_pong_listener() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn answer_pong(listener: std::net::TcpListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 64];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"+PONG\r\n");
            }
        })
    }

    #[test]
    fn live_discovery_endpoint_resolves() {
        let (listener, port) = spawn_pong_listener();
        let responder = answer_pong(listener);
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
            "a live PONG-answering endpoint is the only way this resolver succeeds"
        );
        responder.join().unwrap();
    }

    #[test]
    fn declined_discovery_is_a_named_refusal() {
        // Bind then DROP the listener — the advertised port is dead, exactly
        // like a discovery file left behind by a crashed contextd. The probe's
        // other decline paths are covered in `lunaris_core::store_discovery`;
        // what matters HERE is that a decline maps to the exit ramp and never
        // to a store URL.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONTEXTD_MOON_URL_FILE),
            format!("moon://127.0.0.1:{port}\n"),
        )
        .unwrap();
        assert_refused_with_exit_ramp(resolve_storage_url_at(&scope(), tmp.path()));
    }
}
