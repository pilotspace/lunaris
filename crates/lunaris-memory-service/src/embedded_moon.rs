//! In-process Moon server lifecycle — ONE definition shared by `lunaris-mcp`
//! and `lunaris-contextd` (promoted here from lunaris-mcp, 2026-07-17, for the
//! contextd embedded-moon unification).
//!
//! Everything here is gated behind the `embedded-moon` feature. When the
//! feature is OFF this module compiles to nothing — the workspace build and
//! `cargo test --workspace` stay light (CLAUDE.md invariant: embedded-moon is
//! NEVER in a default feature set).
//!
//! ## Architecture
//!
//! [`launch_embedded_moon`] binds a free loopback port, constructs a
//! `ServerConfig`, spawns `moon_server::server::embedded::run_embedded` on the
//! current tokio runtime, and polls RESP readiness (PING → PONG) with a 5 s
//! hard timeout + exponential backoff. On success it returns an
//! [`EmbeddedMoonGuard`] that owns the `CancellationToken` and `JoinHandle`.
//!
//! [`decide_storage_with_launcher`] is the DI seam: it accepts a closure so
//! tests can inject stub launchers without mutating environment variables
//! (which are `unsafe fn` in edition 2024 and forbidden under
//! `#![forbid(unsafe_code)]`).
//!
//! ## Lock discipline (CLAUDE.md)
//!
//! [`EmbeddedMoonGuard::shutdown`] takes the `JoinHandle` out of the `Mutex`
//! in a single `.lock().take()` call, letting the guard drop before the
//! `.await`. No lock is ever held across an `.await`.

// ── Guard struct ──────────────────────────────────────────────────────────────

/// Owns the in-process Moon server: its `CancellationToken` and `JoinHandle`.
///
/// NOT `Clone` — wrapping callers use `Arc<EmbeddedMoonGuard>` so that a
/// `Clone` state struct can hold an `Option<Arc<Self>>`.
///
/// `Drop` fires `cancel()` (synchronous) and `abort()` (synchronous) as a
/// best-effort cleanup. Async callers should prefer [`Self::shutdown`] which
/// awaits the task with a 3 second hard timeout.
pub struct EmbeddedMoonGuard {
    token: moon_server::runtime::cancel::CancellationToken,
    /// pub so caller-crate tests can assert `handle.lock().is_none()` after
    /// `shutdown()`.
    pub handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
    pub port: u16,
}

/// Manual `Debug` — `CancellationToken` only derives `Clone`, not `Debug`.
impl std::fmt::Debug for EmbeddedMoonGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedMoonGuard")
            .field("port", &self.port)
            .field("cancelled", &self.token.is_cancelled())
            .finish()
    }
}

/// Synchronous best-effort cleanup.
///
/// `cancel()` is synchronous (atomic store + wake). `get_mut()` is available
/// because we have `&mut self` — no lock needed in `Drop`.
impl Drop for EmbeddedMoonGuard {
    fn drop(&mut self) {
        if !self.token.is_cancelled() {
            self.token.cancel();
        }
        // get_mut() returns &mut Option<JoinHandle> directly — no lock acquisition.
        if let Some(h) =
            self.handle.get_mut().take() as Option<tokio::task::JoinHandle<anyhow::Result<()>>>
        {
            h.abort();
        }
    }
}

impl EmbeddedMoonGuard {
    /// Fire cancellation and await the `run_embedded` task with a 3 second hard
    /// timeout.
    ///
    /// The `JoinHandle` is taken out of the `Mutex` and the lock guard is
    /// dropped **before** the `.await` — CLAUDE.md mandate: never hold a lock
    /// across `.await`.
    ///
    /// A second call is a clean no-op: the handle cell is `None` after the
    /// first call, so the timeout/await is skipped entirely.
    pub async fn shutdown(&self) {
        self.token.cancel();
        // Take the handle while holding the lock, then drop the lock guard
        // BEFORE awaiting — lock guard is dropped at the end of this line.
        let handle = self.handle.lock().take();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), h).await;
        }
    }

    /// Returns `true` if the cancellation token has been fired.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum EmbeddedMoonError {
    #[error("port allocation failed: {0}")]
    PortAlloc(#[from] std::io::Error),
    #[error("embedded Moon did not accept connections on :{port} within 5s (dir={data_dir:?})")]
    Timeout { port: u16, data_dir: String },
}

// ── DI seam ───────────────────────────────────────────────────────────────────

/// Resolve the storage URL and optionally launch an embedded Moon instance.
///
/// **This is the testable DI seam.** Production code calls it with the real
/// [`launch_embedded_moon`] closure; tests inject stubs.
///
/// Rules:
/// - If `override_` is `Some`, return it verbatim and **never** call
///   `launcher`. This implements an explicit `--storage <url>` /
///   `LUNARIS_STORE_URL` bypass regardless of the feature flag.
/// - If `launcher` returns `Ok(guard)`, return
///   `("moon://127.0.0.1:<port>", Some(guard))`.
/// - If `launcher` returns `Err(e)`, emit `tracing::warn` (circuit-breaker),
///   fall back to `sqlite:///<HOME>/.lunaris/<scope>.db`, and return
///   `(url, None)`. The caller **always starts** — Moon bring-up failure is
///   NOT fatal.
///
/// No `std::env::set_var` / `remove_var` anywhere — those are `unsafe fn` in
/// edition 2024 and forbidden by the callers' `#![forbid(unsafe_code)]`.
pub async fn decide_storage_with_launcher<F, Fut>(
    override_: Option<&str>,
    scope: &lunaris_core::Scope,
    launcher: F,
) -> (String, Option<EmbeddedMoonGuard>)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<EmbeddedMoonGuard, EmbeddedMoonError>>,
{
    if let Some(url) = override_ {
        // Explicit storage override: bypass embedded Moon entirely.
        return (url.to_owned(), None);
    }

    match launcher().await {
        Ok(guard) => {
            let url = format!("moon://127.0.0.1:{}", guard.port);
            (url, Some(guard))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "embedded Moon launch failed — circuit-breaking to SQLite default"
            );
            // Replicate the SQLite URL construction from the callers'
            // resolve_storage_url helpers (avoids a circular dep). Ensure the
            // parent directory exists before returning the fallback URL —
            // without this, Lunaris::open would fail on a fresh host where
            // ~/.lunaris has not been created yet.
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let dir = home.join(".lunaris");
            // Best-effort dir creation — if it fails, Lunaris::open will
            // surface the error with a better diagnostic than a panic here.
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("{}.db", scope.as_str()));
            (format!("sqlite://{}", path.display()), None)
        }
    }
}

// ── Real launcher ─────────────────────────────────────────────────────────────

/// Launch an in-process Moon server and return a guard that owns it.
///
/// **Port allocation:** Bind `:0` to find a free loopback port, drop the
/// listener, then hand the port to `ServerConfig`. There is a narrow TOCTOU
/// window between dropping and Moon binding — if Moon fails to bind, the
/// readiness poll will time out and the caller circuit-breaks to SQLite.
///
/// **Readiness:** raw RESP PING probe with 5 second hard deadline and
/// exponential backoff (50 ms → 200 ms cap). TCP accept does NOT prove RESP is
/// being served — only a `+PONG` reply does. NEVER holds a lock across
/// `.await`.
pub async fn launch_embedded_moon(data_dir: &str) -> Result<EmbeddedMoonGuard, EmbeddedMoonError> {
    // Step 1: bind :0 to get a free loopback port; drop the listener immediately.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(EmbeddedMoonError::PortAlloc)?;
        listener.local_addr().map_err(EmbeddedMoonError::PortAlloc)?.port()
        // listener drops here, freeing the port
    };

    // Step 2: build ServerConfig via parse_from([]) to get all clap arg defaults,
    // then override only what we need.
    //
    // CRITICAL: ServerConfig::default() (Rust Default trait) gives databases=0
    // and shards=0 because #[derive(Default)] uses Rust's numeric default (0),
    // NOT the clap #[arg(default_value_t)] values. Only
    // ServerConfig::parse_from([]) gives the clap-specified defaults
    // (databases=16, shards=1, appendonly="yes", etc.).
    //
    // With databases=0: Shard::with_initial_keyspace_hint creates 0 databases
    // per shard → ShardDatabases::db_count=0 → timers::run_active_expiry panics
    // on write_db(shard_id, 0) ("db_index 0 out of bounds (0)").
    //
    // With shards=0: run_embedded auto-detects CPU cores (all_parallelism),
    // spawning many shard threads instead of 1 — unnecessary overhead for an
    // embedded instance.
    use clap::Parser;
    let mut config = moon_server::config::ServerConfig::parse_from::<[&str; 0], &str>([]);
    config.bind = "127.0.0.1".to_string();
    config.port = port;
    config.dir = data_dir.to_string();

    // Step 3: spawn run_embedded on the current tokio runtime.
    let token = moon_server::runtime::cancel::CancellationToken::new();
    let handle = tokio::spawn(moon_server::server::embedded::run_embedded(config, token.clone()));

    // Step 4: readiness poll — RESP PING probe with bounded retries +
    // exponential backoff. Hard limit: 5 seconds total. Interval: 50 ms →
    // 100 ms → 200 ms (capped).
    let addr = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut interval = std::time::Duration::from_millis(50);
    let ready = loop {
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        // Try to connect AND send a PING within 200 ms.
        let probe_result = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(&addr).await?;
            // RESP PING: "*1\r\n$4\r\nPING\r\n"
            stream.write_all(b"*1\r\n$4\r\nPING\r\n").await?;
            let mut buf = [0u8; 7]; // "+PONG\r\n"
            stream.read_exact(&mut buf).await?;
            Ok::<bool, std::io::Error>(buf.starts_with(b"+PONG"))
        })
        .await;
        match probe_result {
            Ok(Ok(true)) => break true,
            _ => {
                tokio::time::sleep(interval).await;
                interval = (interval * 2).min(std::time::Duration::from_millis(200));
            }
        }
    };

    if !ready {
        token.cancel();
        handle.abort();
        return Err(EmbeddedMoonError::Timeout { port, data_dir: data_dir.to_string() });
    }

    tracing::info!(port, data_dir, "embedded Moon ready on loopback");
    Ok(EmbeddedMoonGuard { token, handle: parking_lot::Mutex::new(Some(handle)), port })
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// The transport-generic tests moved here with the module (from lunaris-mcp).
// The AppState-coupled round-trip / handover tests stay in lunaris-mcp — they
// exercise mcp-side wiring (state, proxy, session pads) through the re-export.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // shutdown takes the handle and second call is a no-op.
    // Discriminating: assert handle cell is None after shutdown (task was
    // taken+awaited), not merely is_cancelled().
    #[tokio::test]
    async fn shutdown_takes_handle_and_second_call_is_noop() {
        let tmpdir = tempfile::tempdir().unwrap();
        let guard = launch_embedded_moon(tmpdir.path().to_str().unwrap())
            .await
            .expect("embedded Moon must start");
        assert!(guard.handle.lock().is_some(), "handle must be present before shutdown");

        guard.shutdown().await;

        assert!(
            guard.handle.lock().is_none(),
            "shutdown must take the JoinHandle — cell must be None after first call"
        );
        assert!(guard.is_cancelled(), "token must be cancelled after shutdown");

        guard.shutdown().await;
        assert!(guard.handle.lock().is_none(), "second shutdown is a no-op");
    }

    // decide_storage_with_launcher opt-out — override bypasses launcher.
    #[tokio::test]
    async fn decide_storage_override_bypasses_launcher() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-optout").unwrap();
        let launcher_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lc = launcher_called.clone();
        let (url, guard) =
            decide_storage_with_launcher(Some("sqlite:///test.db"), &scope, move || {
                lc.store(true, std::sync::atomic::Ordering::Relaxed);
                async move {
                    Err(EmbeddedMoonError::Timeout { port: 0, data_dir: "unused".into() })
                }
            })
            .await;
        assert_eq!(url, "sqlite:///test.db");
        assert!(guard.is_none(), "override must produce no guard");
        assert!(
            !launcher_called.load(std::sync::atomic::Ordering::Relaxed),
            "launcher must NOT be called when override is given"
        );
    }

    // decide_storage_with_launcher fallback — Err from launcher → sqlite URL.
    // No env mutation needed: the DI seam accepts a stub closure.
    #[tokio::test]
    async fn decide_storage_launcher_failure_falls_back_to_sqlite() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-fallback").unwrap();
        let (url, guard) = decide_storage_with_launcher(None, &scope, || async move {
            Err(EmbeddedMoonError::Timeout { port: 0, data_dir: "simulated".into() })
        })
        .await;
        assert!(url.starts_with("sqlite://"), "fallback URL must be sqlite://, got: {url}");
        assert!(guard.is_none(), "failed launch must produce no guard");
    }

    // decide_storage_with_launcher happy path — real launcher → moon:// URL.
    #[tokio::test]
    async fn decide_storage_real_launcher_returns_moon_url() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-happypath").unwrap();
        let tmpdir = tempfile::tempdir().unwrap();
        let data_dir = tmpdir.path().to_str().unwrap().to_owned();
        let (url, guard) = decide_storage_with_launcher(None, &scope, move || {
            let dir = data_dir.clone();
            async move { launch_embedded_moon(&dir).await }
        })
        .await;
        assert!(url.starts_with("moon://"), "real launcher must produce moon:// URL, got: {url}");
        assert!(guard.is_some(), "real launcher must produce a guard");
        if let Some(g) = guard {
            g.shutdown().await;
        }
    }

    // W3 (moon-v051-perf-exploit) — launch-config refresh against the v0.5.1+
    // flag set: `parse_from([])` must yield the clap defaults (never
    // `Default::default()`'s zeros) and embedded-moon must not force any flag
    // off its upstream default.
    #[test]
    fn server_config_new_v051_flags_have_sane_defaults() {
        use clap::Parser;
        let config = moon_server::config::ServerConfig::parse_from::<[&str; 0], &str>([]);

        assert_eq!(
            config.wal_kv_log, "auto",
            "embedded-moon must inherit the v0.5.1 `auto` wal-kv-log default \
             (never force \"on\", which would reintroduce the AOF+WAL KV \
             double-write PR #211 eliminated)"
        );
        assert_eq!(
            config.mem_full_pct, 95,
            "RSS watchdog mem-full-pct must keep its upstream default"
        );
        assert_eq!(
            config.io_busy_poll_us, 0,
            "io-busy-poll-us must stay disabled by default — busy-polling \
             regresses shared/unpinned cores (embedded-moon runs in-process \
             alongside the host server, never on dedicated cores)"
        );
        assert_eq!(
            config.disk_offload, "enable",
            "disk-offload must keep its upstream default (tiered RAM->mmap->NVMe storage)"
        );
        assert_eq!(
            config.disk_free_min_pct, 5,
            "disk-free-min-pct must keep its upstream default (5% diskfull write-pause guard)"
        );

        assert_eq!(
            config.databases, 16,
            "databases must be the clap default, not Default::default()'s 0"
        );
        assert_eq!(config.shards, 1, "shards must be the clap default, not Default::default()'s 0");
    }
}
