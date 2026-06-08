//! In-process Moon server lifecycle for `lunaris-mcp`.
//!
//! All items in this module are gated behind `#[cfg(feature = "embedded-moon")]`.
//! When the feature is OFF, this module compiles to nothing — the workspace build
//! and `cargo test --workspace` stay light.
//!
//! ## Architecture
//!
//! `launch_embedded_moon` binds a free loopback port, constructs a `ServerConfig`,
//! spawns `moon_server::server::embedded::run_embedded` on the current tokio
//! runtime, and polls TCP readiness with a 5 second hard timeout + exponential
//! backoff. On success it returns an `EmbeddedMoonGuard` that owns the
//! `CancellationToken` and `JoinHandle`.
//!
//! `decide_storage_with_launcher` is the DI seam: it accepts a closure so tests
//! can inject stub launchers without mutating environment variables (which are
//! `unsafe fn` in edition 2024 and forbidden by `#![forbid(unsafe_code)]`).
//!
//! ## Lock discipline (CLAUDE.md)
//!
//! `EmbeddedMoonGuard::shutdown` takes the `JoinHandle` out of the `Mutex` in a
//! single `.lock().take()` call, letting the guard drop before the `.await`.
//! No lock is ever held across an `.await`.

// ── Guard struct ──────────────────────────────────────────────────────────────

/// Owns the in-process Moon server: its `CancellationToken` and `JoinHandle`.
///
/// NOT `Clone` — wrapping callers use `Arc<EmbeddedMoonGuard>` so that
/// `AppState` (which must be `Clone`) can hold a `Option<Arc<Self>>`.
///
/// `Drop` fires `cancel()` (synchronous) and `abort()` (synchronous) as a
/// best-effort cleanup. Async callers should prefer `shutdown()` which awaits
/// the task with a 3 second hard timeout.
#[cfg(feature = "embedded-moon")]
pub(crate) struct EmbeddedMoonGuard {
    token: moon_server::runtime::cancel::CancellationToken,
    // pub(crate) so that tests in state.rs can assert handle.lock().is_none()
    // after shutdown(). Only same-crate code can reach the field.
    pub(crate) handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
    pub(crate) port: u16,
}

/// Manual `Debug` — `CancellationToken` only derives `Clone`, not `Debug`.
#[cfg(feature = "embedded-moon")]
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
#[cfg(feature = "embedded-moon")]
impl Drop for EmbeddedMoonGuard {
    fn drop(&mut self) {
        if !self.token.is_cancelled() {
            self.token.cancel();
        }
        // get_mut() returns &mut Option<JoinHandle> directly — no lock acquisition.
        if let Some(h) = self.handle.get_mut().take() as Option<tokio::task::JoinHandle<anyhow::Result<()>>> {
            h.abort();
        }
    }
}

#[cfg(feature = "embedded-moon")]
impl EmbeddedMoonGuard {
    /// Fire cancellation and await the `run_embedded` task with a 3 second hard
    /// timeout.
    ///
    /// The `JoinHandle` is taken out of the `Mutex` and the lock guard is dropped
    /// **before** the `.await` — CLAUDE.md mandate: never hold a lock across
    /// `.await`.
    ///
    /// A second call is a clean no-op: the handle cell is `None` after the first
    /// call, so the timeout/await is skipped entirely.
    pub(crate) async fn shutdown(&self) {
        self.token.cancel();
        // Take the handle while holding the lock, then drop the lock guard
        // BEFORE awaiting — lock guard is dropped at the end of this line.
        let handle = self.handle.lock().take();
        if let Some(h) = handle {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), h).await;
        }
    }

    /// Returns `true` if the cancellation token has been fired.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

#[cfg(feature = "embedded-moon")]
#[derive(Debug, thiserror::Error)]
pub(crate) enum EmbeddedMoonError {
    #[error("port allocation failed: {0}")]
    PortAlloc(#[from] std::io::Error),
    #[error(
        "embedded Moon did not accept connections on :{port} within 5s (dir={data_dir:?})"
    )]
    Timeout { port: u16, data_dir: String },
}

// ── DI seam ───────────────────────────────────────────────────────────────────

/// Resolve the storage URL and optionally launch an embedded Moon instance.
///
/// **This is the testable DI seam.** Production code calls it with the real
/// `launch_embedded_moon` closure; tests inject stubs.
///
/// Rules:
/// - If `override_` is `Some`, return it verbatim and **never** call `launcher`.
///   This implements the `--storage <url>` bypass regardless of the feature flag.
/// - If `launcher` returns `Ok(guard)`, return `("moon://127.0.0.1:<port>", Some(guard))`.
/// - If `launcher` returns `Err(e)`, emit `tracing::warn` (circuit-breaker),
///   fall back to `sqlite:///<HOME>/.lunaris/<scope>.db`, and return `(url, None)`.
///   The MCP server **always starts** — Moon bring-up failure is NOT fatal
///   (mitigates T-vuz-05).
///
/// No `std::env::set_var` / `remove_var` anywhere — those are `unsafe fn` in
/// edition 2024 and forbidden by `#![forbid(unsafe_code)]` at `main.rs:16`.
#[cfg(feature = "embedded-moon")]
pub(crate) async fn decide_storage_with_launcher<F, Fut>(
    override_: Option<&str>,
    scope: &lunaris_core::Scope,
    launcher: F,
) -> (String, Option<EmbeddedMoonGuard>)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<EmbeddedMoonGuard, EmbeddedMoonError>>,
{
    if let Some(url) = override_ {
        // Explicit --storage override: bypass embedded Moon entirely.
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
            // Replicate the SQLite URL construction from resolve_storage_url
            // (avoids a circular dep on state.rs private helper).
            // [Rule 2] Ensure the parent directory exists before returning the
            // fallback URL — without this, Lunaris::open would fail on a fresh
            // host where ~/.lunaris has not been created yet.
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let dir = home.join(".lunaris");
            // Best-effort dir creation — if it fails, Lunaris::open will surface
            // the error with a better diagnostic than a panic here.
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
/// readiness poll will time out and we circuit-break to SQLite.
///
/// **Readiness:** TCP-connect probe with 5 second hard deadline and exponential
/// backoff (50 ms → 200 ms cap). NEVER holds a lock across `.await`.
#[cfg(feature = "embedded-moon")]
pub(crate) async fn launch_embedded_moon(
    data_dir: &str,
) -> Result<EmbeddedMoonGuard, EmbeddedMoonError> {
    // Step 1: bind :0 to get a free loopback port; drop the listener immediately.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(EmbeddedMoonError::PortAlloc)?;
        listener.local_addr().map_err(EmbeddedMoonError::PortAlloc)?.port()
        // listener drops here, freeing the port
    };

    // Step 2: build ServerConfig via Default — only override what differs.
    // appendonly="yes" is already the default (durability on).
    // admin_port=0 is already the default (no admin HTTP server).
    // shards=1 is already the default.
    // Non-empty dir overrides resolve_dir auto-resolution (we want explicit).
    let config = moon_server::config::ServerConfig {
        bind: "127.0.0.1".to_string(),
        port,
        dir: data_dir.to_string(),
        ..moon_server::config::ServerConfig::default()
    };

    // Step 3: spawn run_embedded on the current tokio runtime.
    let token = moon_server::runtime::cancel::CancellationToken::new();
    let handle = tokio::spawn(moon_server::server::embedded::run_embedded(
        config,
        token.clone(),
    ));

    // Step 4: readiness poll — TCP connect with bounded retries + exponential backoff.
    // Hard limit: 5 seconds total. Interval: 50ms → 100ms → 200ms (capped at 200ms).
    // NEVER hold a lock across .await.
    let addr = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut interval = std::time::Duration::from_millis(50);
    let ready = loop {
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(_)) => break true,
            _ => {
                tokio::time::sleep(interval).await;
                interval = (interval * 2).min(std::time::Duration::from_millis(200));
            }
        }
    };

    if !ready {
        token.cancel();
        handle.abort();
        return Err(EmbeddedMoonError::Timeout {
            port,
            data_dir: data_dir.to_string(),
        });
    }

    tracing::info!(port, data_dir, "embedded Moon ready on loopback");
    Ok(EmbeddedMoonGuard {
        token,
        handle: parking_lot::Mutex::new(Some(handle)),
        port,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "embedded-moon"))]
mod tests {
    use super::*;
    use std::sync::Arc;

    // RED TEST: shutdown takes the handle and second call is a no-op.
    // Discriminating: assert handle cell is None after shutdown (task was taken+awaited),
    // not merely is_cancelled().
    #[tokio::test]
    async fn shutdown_takes_handle_and_second_call_is_noop() {
        let tmpdir = tempfile::tempdir().unwrap();
        let guard = launch_embedded_moon(tmpdir.path().to_str().unwrap())
            .await
            .expect("embedded Moon must start");
        assert!(guard.handle.lock().is_some(), "handle must be present before shutdown");

        guard.shutdown().await;

        // Discriminating: the handle was taken (awaited), cell is now None
        assert!(
            guard.handle.lock().is_none(),
            "shutdown must take the JoinHandle — cell must be None after first call"
        );
        assert!(guard.is_cancelled(), "token must be cancelled after shutdown");

        // Second shutdown must be a clean no-op (no panic, no double-await)
        guard.shutdown().await;
        assert!(guard.handle.lock().is_none(), "second shutdown is a no-op");
    }

    // RED TEST: decide_storage_with_launcher opt-out — override bypasses launcher.
    #[tokio::test]
    async fn decide_storage_override_bypasses_launcher() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-optout").unwrap();
        let launcher_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lc = launcher_called.clone();
        let (url, guard) = decide_storage_with_launcher(
            Some("sqlite:///test.db"),
            &scope,
            move || {
                lc.store(true, std::sync::atomic::Ordering::Relaxed);
                async move {
                    Err(EmbeddedMoonError::Timeout {
                        port: 0,
                        data_dir: "unused".into(),
                    })
                }
            },
        )
        .await;
        assert_eq!(url, "sqlite:///test.db");
        assert!(guard.is_none(), "override must produce no guard");
        assert!(
            !launcher_called.load(std::sync::atomic::Ordering::Relaxed),
            "launcher must NOT be called when override is given"
        );
    }

    // RED TEST: decide_storage_with_launcher fallback — Err from launcher → sqlite URL.
    // No env mutation needed: the DI seam accepts a stub closure.
    #[tokio::test]
    async fn decide_storage_launcher_failure_falls_back_to_sqlite() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-fallback").unwrap();
        let (url, guard) = decide_storage_with_launcher(
            None,
            &scope,
            || async move {
                Err(EmbeddedMoonError::Timeout {
                    port: 0,
                    data_dir: "simulated".into(),
                })
            },
        )
        .await;
        assert!(
            url.starts_with("sqlite://"),
            "fallback URL must be sqlite://, got: {url}"
        );
        assert!(guard.is_none(), "failed launch must produce no guard");
    }

    // RED TEST: decide_storage_with_launcher happy path — real launcher → moon:// URL.
    #[tokio::test]
    async fn decide_storage_real_launcher_returns_moon_url() {
        use lunaris_core::Scope;
        let scope = Scope::new("test-vuz-happypath").unwrap();
        let tmpdir = tempfile::tempdir().unwrap();
        let data_dir = tmpdir.path().to_str().unwrap().to_owned();
        let (url, guard) = decide_storage_with_launcher(
            None,
            &scope,
            move || {
                // Clone into the async block so it doesn't borrow from the closure env.
                let dir = data_dir.clone();
                async move { launch_embedded_moon(&dir).await }
            },
        )
        .await;
        assert!(
            url.starts_with("moon://"),
            "real launcher must produce moon:// URL, got: {url}"
        );
        assert!(guard.is_some(), "real launcher must produce a guard");
        if let Some(g) = guard {
            g.shutdown().await;
        }
    }

    // RED TEST: round-trip through live Moon.
    // This test references AppState { _embedded_moon } which does not yet exist.
    // It will fail to COMPILE until T2 adds the field to AppState.
    // After T2 it turns GREEN. This is the expected RED state.
    //
    // Drive the launcher directly (no embedder probe — use open_with_embedder +
    // StubEmbedder to avoid a GGUF download).
    #[tokio::test]
    async fn embedded_moon_scratchpad_round_trip() {
        use lunaris::Lunaris;
        use lunaris_core::{Scope, StubEmbedder};

        let tmpdir = tempfile::tempdir().unwrap();
        let guard = launch_embedded_moon(tmpdir.path().to_str().unwrap())
            .await
            .expect("embedded Moon must start");
        let url = format!("moon://127.0.0.1:{}", guard.port);

        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder(&url, embedder)
            .await
            .expect("Lunaris::open must succeed with live Moon");

        let scope = Scope::new("test-vuz-roundtrip").unwrap();
        let app_state = crate::state::AppState {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None, // guard owned separately for this test
        };

        // write then read
        let write_params = crate::tools::scratchpad_write::ScratchpadWriteParams {
            key: "hello".into(),
            value: serde_json::json!("world"),
            namespace: None,
        };
        let write_resp = crate::tools::scratchpad_write::handle(&app_state, write_params)
            .await
            .expect("scratchpad_write must succeed");
        assert!(!write_resp.lsn.is_empty());

        let read_params = crate::tools::scratchpad_read::ScratchpadReadParams {
            key: "hello".into(),
            namespace: None,
        };
        let read_resp = crate::tools::scratchpad_read::handle(&app_state, read_params)
            .await
            .expect("scratchpad_read must succeed");

        assert!(read_resp.found, "key written to Moon must be found on read");
        assert_eq!(
            read_resp.value,
            Some(serde_json::json!("world")),
            "read value must match written value"
        );

        // cleanup
        let guard_arc = Arc::new(guard);
        guard_arc.shutdown().await;
    }
}
