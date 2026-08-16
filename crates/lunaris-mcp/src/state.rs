//! Shared application state for the Lunaris MCP server.
//!
//! `AppState` holds an `Arc<Lunaris>` handle and the resolved `Scope` for
//! this server process. The scope is bound at startup; wire payloads cannot
//! override it (CLAUDE.md §JWT/scope discipline applies here too — the stdio
//! transport is process-bound, so the scope comes from the CLI/env, never
//! from client-supplied tool arguments).
//!
//! `ScopedLunaris<'_>` borrows from `&Lunaris` and must NOT be stored in
//! state. Re-derive it per tool call: `state.lunaris.scoped(state.scope.clone())`.

use std::{io, sync::Arc};

use lunaris::Lunaris;
use lunaris_core::{LunarisError, Scope};
use thiserror::Error;

// ── Error ────────────────────────────────────────────────────────────────────

/// Errors that can occur during MCP server bootstrap.
///
/// Returned by [`AppState::bootstrap`] — callers map this to an
/// `anyhow::Error` in `main.rs` so the process exits with a diagnostic.
#[derive(Debug, Error)]
pub(crate) enum BootstrapError {
    /// Scope resolution failed (bad override, no $HOME, or I/O on scopes.json).
    #[error("scope resolution: {0}")]
    Scope(#[from] crate::scope_resolver::ScopeResolveError),

    /// `Lunaris::open` returned an error (bad URL, DB migration failure, etc.).
    #[error("lunaris open: {0}")]
    LunarisOpen(#[from] LunarisError),

    /// Filesystem I/O failure while deriving the default storage path.
    #[error("storage path i/o: {0}")]
    Io(#[from] io::Error),

    /// No storage URL was supplied and none could be derived.
    ///
    /// 0.7.0 removed the per-scope SQLite default, so `--storage` /
    /// `LUNARIS_MCP_STORAGE` is now mandatory on a stock build. Carries
    /// [`NO_STORAGE_HELP`].
    #[error("{0}")]
    NoStorage(String),

    /// The `embedded-moon` build could not bring its in-process Moon up.
    ///
    /// Only reachable on `--features embedded-moon` (dev/test only — CLAUDE.md
    /// invariant keeps it out of every default feature set) with no
    /// `--storage` override.
    #[cfg(feature = "embedded-moon")]
    #[error("embedded Moon launch failed: {0}")]
    EmbeddedMoon(#[from] crate::embedded_moon::EmbeddedMoonError),
}

/// The operator exit ramp printed when `lunaris-mcp` starts with no storage.
///
/// Held as one constant so the stock build and any future caller cannot drift
/// into telling an operator two different stories about the same startup.
pub(crate) const NO_STORAGE_HELP: &str = "\
lunaris-mcp needs a storage URL and has no default: pass `--storage \
moon://host:port` or set `LUNARIS_MCP_STORAGE`. Alternatively, run \
`lunaris-contextd` — a live contextd advertises its store in \
`~/.lunaris/contextd-moon.url` and this server adopts it (after a liveness \
probe), which is also how `lunaris-hook` finds the same Moon.\n\n\
Through 0.6.x an unset value silently opened a per-scope SQLite file at \
`~/.lunaris/<scope>.db`. 0.7.0 is Moon-only — that backend is gone, and \
guessing a Moon endpoint is worse than refusing: `moon://127.0.0.1:6380` may \
belong to an unrelated store, and an MCP server that mis-routes an agent's \
memory is harder to notice than one that will not start.\n\n\
Stand a Moon up:\n  \
curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | sh\n  \
moon --bind 127.0.0.1 --port 6380 --shards 1 --dir ~/.lunaris/moon\n\
then start with `--storage moon://127.0.0.1:6380`. Moon MUST run with \
`--shards 1` (Lunaris ingest is a single-shard TXN). Full recipe — durability, \
health checks, container flags: docs/operations/external-moon.md.\n\n\
Migrating an existing 0.6.x SQLite/Postgres store? Run `lunaris-migrate` from \
the v0.6.2 release binary BEFORE upgrading — see docs/migration/0.6-to-0.7.md.";

/// Appended to [`NO_STORAGE_HELP`] when a discovery file WAS present but did
/// not pass the probe.
///
/// Without this line the two cases are indistinguishable to an operator who
/// just started contextd and is staring at "needs a storage URL" — and the
/// fixes differ: "start contextd" vs "contextd died and left this file
/// behind". Discovery is read once at boot, so the restart matters.
#[cfg(not(feature = "embedded-moon"))]
pub(crate) const STALE_DISCOVERY_NOTE: &str = "\
NOTE: `~/.lunaris/contextd-moon.url` exists but did not answer a RESP PING on \
loopback within the probe budget, so it was NOT trusted (a crashed contextd \
leaves this file behind, and its ephemeral port is eventually reused by an \
unrelated process). Restart `lunaris-contextd`, delete the stale file, or set \
`LUNARIS_MCP_STORAGE` explicitly. The file is read ONCE at boot — starting \
contextd afterwards does not re-point a running server.";

// ── State ────────────────────────────────────────────────────────────────────

/// Shared, cheaply-cloneable application state injected into every tool handler.
///
/// `lunaris` is `Arc`-wrapped so cloning `AppState` does not clone the engine.
/// `scope` is a validated newtype; cloning it per tool call is intentional and
/// cheap (it is a `String` behind an `Arc` in `Scope`'s impl).
///
/// **Do not store `ScopedLunaris<'_>` in this struct** — its lifetime borrows
/// from `&'_ Lunaris`, which cannot outlive the stack frame of a tool handler.
#[derive(Debug, Clone)]
pub(crate) struct AppState {
    /// High-level Lunaris engine handle (embedder + clock + storage wired in).
    pub(crate) lunaris: Arc<Lunaris>,
    /// Scope bound at server startup — all memory operations partition by this.
    pub(crate) scope: Scope,
    /// The URL `lunaris` was opened against.
    ///
    /// Retained (it used to be a local in `bootstrap_inner`) so the proxy can
    /// compare it against the store `lunaris-contextd` reports over the socket.
    /// Until they were compared, falling back to Direct could silently continue
    /// an op stream in a second Moon (split-routing containment, task #20).
    ///
    /// Whatever [`resolve_storage_url`] returned: `--storage` /
    /// `LUNARIS_MCP_STORAGE`, or (task #28) the store contextd advertised in
    /// `~/.lunaris/contextd-moon.url`. On the discovery path the comparison is
    /// trivially satisfied — both sides read the same file — which is the
    /// point: the split-check only has work to do when an operator configured
    /// the two halves separately.
    pub(crate) storage_url: String,
    /// Owned embedded Moon guard — keeps the in-process Moon alive for the
    /// server's lifetime. `None` when the `embedded-moon` feature is OFF, when
    /// a `--storage` override was supplied, or when Moon bring-up failed and the
    /// circuit-breaker fell back to SQLite. `Arc<>` restores `Clone` on
    /// `AppState` (the inner `Mutex<Option<JoinHandle>>` is not `Clone`).
    #[cfg(feature = "embedded-moon")]
    pub(crate) _embedded_moon: Option<Arc<crate::embedded_moon::EmbeddedMoonGuard>>,
}

impl AppState {
    /// Build `AppState` from CLI arguments.
    ///
    /// Steps:
    /// 1. Resolve the active scope via [`crate::scope_resolver::resolve`].
    /// 2. Derive or use the caller-supplied storage URL.
    /// 3. Open the Lunaris engine (async; may run DB migrations) with the
    ///    **lazy** default embedder — NO weights load at boot.
    ///
    /// Unified-inference contract (2026-07-19): embed-needing ops are normally
    /// served by the warm `lunaris-contextd` daemon over its socket, so this
    /// process must not park a second resident copy of the GGUF weights. The
    /// engine's embedder is [`lunaris::lazy_default_embedder`]: the resolve
    /// chain (GGUF → remote → Noop) only runs on the first in-process
    /// `embed_batch` — i.e. only when an embed op is genuinely served Direct
    /// (standalone installs, contextd unreachable). A Noop resolution surfaces
    /// as an actionable error on that first call, not as silent empty hits —
    /// which is why the old boot-time health probe (and its
    /// `LUNARIS_MCP_SKIP_EMBEDDER_PROBE` escape hatch) no longer exists.
    pub(crate) async fn bootstrap(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
    ) -> Result<Self, BootstrapError> {
        Self::bootstrap_inner(scope_override, storage_override, None).await
    }

    /// Internal bootstrap with a test-only data-dir override for embedded-moon.
    pub(crate) async fn bootstrap_inner(
        scope_override: Option<&str>,
        storage_override: Option<&str>,
        // Used only by the #[cfg(feature = "embedded-moon")] block below.
        // Suppressed on non-embedded-moon builds where only the cfg(not) branch runs.
        #[cfg_attr(not(feature = "embedded-moon"), allow(unused_variables))]
        data_dir_override: Option<&str>,
    ) -> Result<Self, BootstrapError> {
        let scope = crate::scope_resolver::resolve(scope_override)?;

        // Derive storage URL — and optionally launch embedded Moon.
        // When the embedded-moon feature is ON and no --storage override is
        // given, decide_storage_with_launcher starts run_embedded in-process
        // and returns the moon://127.0.0.1:<port> URL. A launch failure is now
        // terminal (0.7.0: there is no SQLite left to circuit-break onto).
        #[cfg(feature = "embedded-moon")]
        let (storage_url, embedded_guard) = {
            let data_dir = data_dir_override.unwrap_or("./.lunaris-moon").to_owned();
            crate::embedded_moon::decide_storage_with_launcher(storage_override, move || {
                // Clone into the async block so the future owns `data_dir`
                // and does not borrow from the closure environment.
                let dir = data_dir.clone();
                async move { crate::embedded_moon::launch_embedded_moon(&dir).await }
            })
            .await?
        };
        // Map Option<EmbeddedMoonGuard> → Option<Arc<EmbeddedMoonGuard>>.
        #[cfg(feature = "embedded-moon")]
        let embedded_guard = embedded_guard.map(Arc::new);

        // When embedded-moon is OFF (every shipped build), the override is the
        // only source of a storage URL.
        #[cfg(not(feature = "embedded-moon"))]
        let storage_url = resolve_storage_url(storage_override)?;

        tracing::info!(
            scope   = scope.as_str(),
            storage = %storage_url,
            "opening lunaris engine",
        );
        // Lazy embedder: no weights load here. If a load ever happens (first
        // Direct embed op), it uses the shared small-budget "interactive"
        // resolve chain, so the llama.cpp context does not reserve the
        // ~2.3 GB worst-case batch buffer (LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS
        // tunes it).
        let embedder = lunaris::lazy_default_embedder();
        let lunaris = Lunaris::open_with_embedder(&storage_url, embedder).await?;

        // P-C (260609-dvi): install ActR consolidator; pipeline stays DISABLED so no
        // background worker is spawned — memory.scratchpad_consolidate is the SOLE
        // consumer. Force-installs regardless of LUNARIS_CONSOLIDATOR_BACKEND env var
        // so the MCP binary always has a real scoring consolidator available on demand.
        lunaris
            .consolidator_pipeline()
            .set_consolidator(
                Arc::new(lunaris::ActRConsolidator::default()) as Arc<dyn lunaris::Consolidator>
            );

        Ok(Self {
            lunaris: Arc::new(lunaris),
            scope,
            storage_url,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: embedded_guard,
        })
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Resolve the storage URL.
///
/// Precedence (task #28):
/// 1. `--storage` / `LUNARIS_MCP_STORAGE` — explicit always wins, passed
///    verbatim.
/// 2. The store a live `lunaris-contextd` **advertises** in
///    `~/.lunaris/contextd-moon.url`, adopted only after the shared
///    loopback + RESP-`PING` liveness probe
///    ([`lunaris_core::store_discovery`], the same function `lunaris-hook`
///    resolves through).
/// 3. [`BootstrapError::NoStorage`] carrying [`NO_STORAGE_HELP`].
///
/// Step 2 is a deliberate revision of the 0.7.0 "no default storage" contract,
/// not a retreat from it: an *advertised and probed* store is not a guess. The
/// alternative — refusing next to a running contextd — pushed operators into
/// configuring one install twice, and a half-configured pair is exactly the
/// split-routing state the proxy now has to refuse to serve through. What
/// stays banned is guessing: no SQLite, and no hardcoded
/// `moon://127.0.0.1:6380`. A dead or tampered discovery file is DECLINED, so
/// it lands in step 3 rather than in somebody else's Moon.
///
/// Read once, at boot: a discovery file that appears mid-session is not picked
/// up (see the [`lunaris_core::store_discovery`] module docs).
///
/// Used only when the `embedded-moon` feature is OFF — which is every shipped
/// `npx`/`uvx`/`cargo install` binary (CLAUDE.md invariant: `embedded-moon` is
/// never in a default feature set). When the feature IS on,
/// `decide_storage_with_launcher` supplies the URL from the Moon it launched,
/// and discovery never runs: that build already owns a store.
#[cfg(not(feature = "embedded-moon"))]
fn resolve_storage_url(override_: Option<&str>) -> Result<String, BootstrapError> {
    let lunaris_dir = dirs::home_dir().map(|home| home.join(".lunaris"));
    resolve_storage_url_at(override_, lunaris_dir.as_deref())
}

/// Testable body of [`resolve_storage_url`] — takes the `~/.lunaris` dir
/// explicitly so tests can point it at a tempdir (no env mutation: `set_var`
/// is an `unsafe fn` in edition 2024). `None` means the home directory could
/// not be determined, which is indistinguishable from "no file" for our
/// purposes: there is nowhere to look.
#[cfg(not(feature = "embedded-moon"))]
fn resolve_storage_url_at(
    override_: Option<&str>,
    lunaris_dir: Option<&std::path::Path>,
) -> Result<String, BootstrapError> {
    use lunaris_core::store_discovery::{StoreDiscovery, discover_contextd_moon};

    if let Some(url) = override_.filter(|u| !u.trim().is_empty()) {
        return Ok(url.to_owned());
    }
    match lunaris_dir.map(discover_contextd_moon) {
        Some(StoreDiscovery::Live(url)) => {
            tracing::info!(storage = %url, "adopted the store advertised by lunaris-contextd");
            Ok(url)
        }
        Some(StoreDiscovery::Declined) => {
            Err(BootstrapError::NoStorage(format!("{NO_STORAGE_HELP}\n\n{STALE_DISCOVERY_NOTE}")))
        }
        Some(StoreDiscovery::Absent) | None => {
            Err(BootstrapError::NoStorage(NO_STORAGE_HELP.to_string()))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // The old boot-time `probe_embedder_health` unit tests lived here. The
    // probe moved into `lunaris::lazy_default_embedder`'s first-load path
    // (unified-inference, 2026-07-19); its honest-error behavior is now pinned
    // end-to-end by `tests/lazy_embedder_boot.rs` against the real binary.

    // ── Storage is required (0.7.0) ──────────────────────────────────────────

    /// A stock build (no `embedded-moon`) with no `--storage` must REFUSE, and
    /// the refusal must be enough to fix the config without reading source.
    ///
    /// The shipped default was a per-scope SQLite file. Deleting that backend
    /// without deleting the default would have left `lunaris-mcp` minting a
    /// `sqlite://` URL for `Lunaris::open` to reject — a scheme error two
    /// frames from the actual decision, naming a path the operator never
    /// chose.
    ///
    /// Hermetic: the lunaris dir is an EMPTY tempdir, so this pins "nothing
    /// explicit and nothing advertised" rather than "whatever this developer's
    /// `~/.lunaris` happens to hold".
    #[cfg(not(feature = "embedded-moon"))]
    #[test]
    fn absent_storage_is_a_named_refusal_with_the_quickstart() {
        let empty = tempfile::tempdir().unwrap();
        let err = resolve_storage_url_at(None, Some(empty.path()))
            .expect_err("no --storage and no advertised store must not resolve a URL");
        assert!(
            matches!(err, BootstrapError::NoStorage(_)),
            "must be the named NoStorage variant, got: {err:?}"
        );
        let msg = err.to_string();
        for needle in [
            "LUNARIS_MCP_STORAGE",
            "--storage",
            "moon://",
            "--shards 1",
            "docs/operations/external-moon.md",
            "lunaris-migrate",
            "v0.6.2",
            // Task #28: contextd is a supported alternative to --storage, and
            // an operator who cannot see this line will not find it.
            "lunaris-contextd",
            "contextd-moon.url",
        ] {
            assert!(msg.contains(needle), "startup error must mention {needle}: {msg}");
        }
        assert!(
            !msg.contains("did not answer a RESP PING"),
            "there was no discovery file — the stale-file note must not appear: {msg}"
        );
    }

    /// Whitespace is not a storage URL. `LUNARIS_MCP_STORAGE=""` in a shell
    /// wrapper reaches clap as `Some("")`, which would otherwise sail through
    /// to `Lunaris::open("")` and fail on URL parsing instead.
    #[cfg(not(feature = "embedded-moon"))]
    #[test]
    fn blank_storage_is_treated_as_absent() {
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_storage_url_at(Some("   "), Some(empty.path())),
            Err(BootstrapError::NoStorage(_))
        ));
        assert_eq!(
            resolve_storage_url_at(Some("moon://h:6380"), Some(empty.path())).unwrap(),
            "moon://h:6380"
        );
    }

    // ── Contextd discovery is arm 2 (task #28) ───────────────────────────────

    /// Stand up a one-shot RESP responder and advertise it in `dir`.
    /// Returns the URL written and the responder thread.
    #[cfg(not(feature = "embedded-moon"))]
    fn advertise_pong_endpoint(dir: &std::path::Path) -> (String, std::thread::JoinHandle<()>) {
        use lunaris_core::store_discovery::CONTEXTD_MOON_URL_FILE;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 64];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"+PONG\r\n");
            }
        });
        let url = format!("moon://127.0.0.1:{port}");
        std::fs::write(dir.join(CONTEXTD_MOON_URL_FILE), format!("{url}\n")).unwrap();
        (url, handle)
    }

    /// A live, advertised contextd store IS configured storage — this is the
    /// unit-level twin of `tests/server_boot.rs::
    /// advertised_contextd_store_boots_the_stock_server`.
    #[cfg(not(feature = "embedded-moon"))]
    #[test]
    fn live_advertised_store_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let (url, responder) = advertise_pong_endpoint(dir.path());
        assert_eq!(resolve_storage_url_at(None, Some(dir.path())).unwrap(), url);
        responder.join().unwrap();
    }

    /// Explicit beats advertised, and must not even probe: an operator who
    /// passed `--storage` has already answered the question.
    #[cfg(not(feature = "embedded-moon"))]
    #[test]
    fn explicit_storage_wins_over_a_live_advertised_store() {
        let dir = tempfile::tempdir().unwrap();
        let (advertised, responder) = advertise_pong_endpoint(dir.path());
        assert_eq!(
            resolve_storage_url_at(Some("moon://db.internal:6380"), Some(dir.path())).unwrap(),
            "moon://db.internal:6380"
        );
        assert_ne!(advertised, "moon://db.internal:6380");
        // Nothing probed, so the responder is still parked on `accept()`.
        // Unblock it ourselves so the thread can be joined instead of leaked.
        let _ = std::net::TcpStream::connect(advertised.trim_start_matches("moon://"));
        responder.join().unwrap();
    }

    /// A discovery file naming a DEAD endpoint is not configured storage. Same
    /// refusal as no file at all, plus the line that tells the operator which
    /// of the two situations they are in.
    #[cfg(not(feature = "embedded-moon"))]
    #[test]
    fn stale_advertised_store_refuses_with_the_stale_note() {
        use lunaris_core::store_discovery::CONTEXTD_MOON_URL_FILE;

        // Bind then DROP — the advertised port is dead, exactly like a file
        // left behind by a crashed contextd.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONTEXTD_MOON_URL_FILE),
            format!("moon://127.0.0.1:{port}\n"),
        )
        .unwrap();

        let err = resolve_storage_url_at(None, Some(dir.path()))
            .expect_err("a dead advertised endpoint must never be treated as configured storage");
        assert!(matches!(err, BootstrapError::NoStorage(_)), "got: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("--storage"), "the quickstart must still be there: {msg}");
        assert!(
            msg.contains("did not answer a RESP PING"),
            "a present-but-declined file must say so — 'needs a storage URL' alone sends the \
             operator hunting for a config they already wrote: {msg}"
        );
    }

    /// No home directory: nowhere to look, ordinary refusal, no panic.
    #[cfg(not(feature = "embedded-moon"))]
    #[test]
    fn no_home_dir_is_the_ordinary_refusal() {
        let err = resolve_storage_url_at(None, None).expect_err("must refuse");
        assert!(matches!(err, BootstrapError::NoStorage(_)), "got: {err:?}");
        assert!(!err.to_string().contains("did not answer a RESP PING"));
    }

    // ── Discriminating bootstrap test (T1) ───────────────────────────────────

    /// Discriminating bootstrap test: driving the REAL production resolution path
    /// (AppState::bootstrap_inner) with --features embedded-moon MUST store an
    /// EmbeddedMoonGuard in _embedded_moon — proving the #[cfg(feature)] branch
    /// launched Moon, not the SQLite circuit-breaker fallback.
    ///
    /// If the cfg dispatch is ever broken (guard dropped early, branch wrong, guard
    /// not stored), _embedded_moon is None and this assertion fires — catching the
    /// built≠wired failure mode before npx/uvx distribution flips embedded-moon on.
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn bootstrap_launches_moon_not_sqlite_fallback() {
        use lunaris_memory_service::scratchpad_read::{
            ScratchpadReadParams, handle as read_handle,
        };
        use lunaris_memory_service::scratchpad_write::{
            ScratchpadWriteParams, handle as write_handle,
        };

        // best-effort cleanup: remove any stale .lunaris-moon from a previous run
        let _ = std::fs::remove_dir_all("./.lunaris-moon");

        let state = AppState::bootstrap_inner(None, None, None)
            .await
            .expect("bootstrap_inner must succeed with --features embedded-moon");

        // THE discriminating assertion: guard must be stored (Moon was launched, not SQLite fallback)
        assert!(
            state._embedded_moon.is_some(),
            "feature=embedded-moon + no --storage override MUST launch embedded Moon and store \
             the guard; _embedded_moon is None, meaning the SQLite circuit-breaker silently took over"
        );

        // Round-trip through the production-constructed state to prove the path is live
        let write_resp = write_handle(
            &state.lunaris,
            &state.scope,
            ScratchpadWriteParams {
                key: "bootstrap-wired".into(),
                value: serde_json::json!("ok"),
                namespace: None,
            },
        )
        .await
        .expect("scratchpad_write on bootstrap-produced state must succeed");
        assert!(!write_resp.lsn.is_empty(), "write response lsn must be non-empty");

        let read_resp = read_handle(
            &state.lunaris,
            &state.scope,
            ScratchpadReadParams { key: "bootstrap-wired".into(), namespace: None },
        )
        .await
        .expect("scratchpad_read on bootstrap-produced state must succeed");
        assert!(read_resp.found, "key written via bootstrap state must be found on read");
        assert_eq!(
            read_resp.value,
            Some(serde_json::json!("ok")),
            "read value must match written value"
        );

        // cleanup
        drop(state);
        let _ = std::fs::remove_dir_all("./.lunaris-moon");
    }

    // ── GREEN tests for embedded-moon DI seam (T2) ────────────────────────────

    /// GREEN: decide_storage_with_launcher happy path + storage URL assertion.
    ///
    /// Calls the DI seam with the real launcher, asserts moon:// URL and that
    /// the handle cell is None after shutdown (task was taken+awaited).
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn decide_storage_real_launcher_wires_moon_url() {
        let tmpdir = tempfile::tempdir().unwrap();
        let data_dir = tmpdir.path().to_str().unwrap().to_owned();
        let (url, guard) = crate::embedded_moon::decide_storage_with_launcher(None, move || {
            let dir = data_dir.clone();
            async move { crate::embedded_moon::launch_embedded_moon(&dir).await }
        })
        .await
        .expect("real launcher must bring Moon up");
        assert!(
            url.starts_with("moon://"),
            "decide_storage with real launcher must return moon:// URL, got: {url}"
        );
        assert!(guard.is_some(), "real launcher must produce a guard");
        if let Some(g) = guard {
            g.shutdown().await;
            assert!(g.handle.lock().is_none(), "handle cell must be None after shutdown");
        }
    }

    /// GREEN: --storage override bypasses embedded Moon entirely.
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn decide_storage_override_skips_embedded_moon() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        let (url, guard) = crate::embedded_moon::decide_storage_with_launcher(
            Some("moon://db.internal:6380"),
            move || {
                c.store(true, Ordering::Relaxed);
                async move {
                    Err(crate::embedded_moon::EmbeddedMoonError::Timeout {
                        port: 0,
                        data_dir: "unused".into(),
                    })
                }
            },
        )
        .await
        .expect("an override never consults the launcher, so it cannot fail");
        assert_eq!(url, "moon://db.internal:6380");
        assert!(guard.is_none(), "--storage override must produce no guard");
        assert!(!called.load(Ordering::Relaxed), "launcher must NOT be called");
    }
}
