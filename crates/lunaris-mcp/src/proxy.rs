//! Socket-first proxy to `lunaris-contextd` with direct-open fallback.
//!
//! contextd-mcp-merge + scratchpad-proxiable. Each engine + scratchpad `#[tool]`
//! builds a [`MemoryRequest`] and hands it here. The proxy is Socket-first: it forwards
//! the request to the warm daemon over its unix socket (connection-per-call,
//! matching the daemon's framing) and returns the daemon's DTO. When the
//! socket is unreachable it trips a circuit breaker after N strikes and serves
//! the call itself via the **identical**
//! [`lunaris_memory_service::protocol::dispatch`] the daemon uses — so the two
//! paths cannot diverge (the safety rule this whole task exists to enforce).
//!
//! Design-for-failure:
//! - connect is bounded by a cold-start budget (`LUNARIS_MCP_CONTEXTD_CONNECT_MS`);
//! - transport failures (connect refused / timeout / decode / empty reply from
//!   an out-of-date daemon) increment a strike counter; at `breaker_n` strikes
//!   the per-session route latches to Direct and logs once;
//! - an application error FROM the daemon (`MemoryResponse::Err`) is authoritative
//!   — the daemon reached the engine, so a direct retry against the same storage
//!   would only repeat it; it is surfaced, and the breaker is reset (transport OK).

use std::path::PathBuf;
use std::sync::Once;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

use lunaris_memory_service::protocol::{CONTEXTD_SOCKET_ENV, MemoryRequest, MemoryResponse};

use crate::state::AppState;

const ROUTE_SOCKET: u8 = 0;
const ROUTE_DIRECT: u8 = 1;

/// Cold-start connect budget. A missing/slow daemon must not stall a tool call
/// past this before falling back.
const DEFAULT_CONNECT_MS: u64 = 500;
/// Consecutive transport failures before the route latches to Direct.
const DEFAULT_BREAKER_N: usize = 3;

/// Per-session Socket→Direct router with a circuit breaker.
#[derive(Debug)]
pub(crate) struct MemoryProxy {
    /// `None` disables the socket path entirely (Direct-only): no daemon
    /// configured, or `LUNARIS_MCP_DISABLE_CONTEXTD` set.
    socket_path: Option<PathBuf>,
    /// Current route (`ROUTE_SOCKET` / `ROUTE_DIRECT`). Latches to Direct once
    /// tripped; never returns to Socket within a session (per §3).
    route: AtomicU8,
    /// Consecutive transport-failure count; reset on any healthy socket reply.
    error_count: AtomicUsize,
    connect_timeout: Duration,
    breaker_n: usize,
    /// One-shot log when the breaker first trips.
    fallback_logged: Once,
}

/// Outcome of a socket attempt.
enum SocketErr {
    /// Connect/timeout/io/decode — transport is unhealthy; trip the breaker and
    /// fall back to Direct for this call.
    Transport(String),
    /// The daemon answered with an engine error — authoritative, surfaced as-is.
    App { code: String, message: String },
}

impl MemoryProxy {
    pub(crate) fn new() -> Self {
        // contextd speaks a unix socket; on non-unix targets the socket leg
        // does not exist, so the proxy is Direct-only from construction
        // (no futile strikes, no breaker latency). The platform question is
        // asked exactly once, here, via the SocketSupport seam.
        let socket_path = resolve_socket_support(
            cfg!(unix),
            // Force Direct-only (tests / air-gapped operators / no daemon wanted).
            std::env::var_os("LUNARIS_MCP_DISABLE_CONTEXTD").is_some(),
            std::env::var_os(CONTEXTD_SOCKET_ENV).map(PathBuf::from),
            dirs::home_dir(),
        )
        .into_socket_path();
        // Socket-first ONLY when the socket file already exists — otherwise skip
        // futile connect attempts on hosts where contextd is not deployed. A
        // daemon that dies mid-session is still caught by the breaker below.
        let initial = match &socket_path {
            Some(p) if p.exists() => ROUTE_SOCKET,
            _ => ROUTE_DIRECT,
        };
        let connect_ms = env_u64("LUNARIS_MCP_CONTEXTD_CONNECT_MS").unwrap_or(DEFAULT_CONNECT_MS);
        let breaker_n = env_usize("LUNARIS_MCP_CONTEXTD_BREAKER_N").unwrap_or(DEFAULT_BREAKER_N);
        Self {
            socket_path,
            route: AtomicU8::new(initial),
            error_count: AtomicUsize::new(0),
            connect_timeout: Duration::from_millis(connect_ms),
            breaker_n: breaker_n.max(1),
            fallback_logged: Once::new(),
        }
    }

    /// Direct-only proxy for cross-module tests (no socket, never routes out).
    /// Used by `tools::staging` tests that exercise session-aware namespace
    /// resolution without depending on a live contextd socket on the host.
    #[cfg(test)]
    pub(crate) fn direct_only_for_test() -> Self {
        Self {
            socket_path: None,
            route: AtomicU8::new(ROUTE_DIRECT),
            error_count: AtomicUsize::new(0),
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_MS),
            breaker_n: 1,
            fallback_logged: Once::new(),
        }
    }

    /// Route one engine op. Socket-first with direct-open fallback.
    pub(crate) async fn dispatch(
        &self,
        state: &AppState,
        req: MemoryRequest,
    ) -> Result<Value, rmcp::ErrorData> {
        // Unified-inference contract (2026-07-19): the mcp engine's embedder is
        // lazy — a Direct serve of an embed-needing op forces a full local
        // weight load (a second resident copy next to contextd's). So embed
        // ops attempt the socket on EVERY call, even after the breaker latched
        // Direct: one cheap connect keeps contextd the single inference host
        // the moment the daemon recovers. Embed-free ops keep the latched
        // Direct fast path.
        let try_socket_first = self.route.load(Ordering::Relaxed) == ROUTE_SOCKET
            || (req.needs_embedder() && self.socket_path.is_some());
        if try_socket_first {
            match self.try_socket(&req).await {
                Ok(data) => {
                    self.error_count.store(0, Ordering::Relaxed);
                    return Ok(data);
                }
                Err(SocketErr::App { code, message }) => {
                    // Transport is healthy; the engine genuinely failed. A direct
                    // retry uses the same storage → same error. Surface it.
                    self.error_count.store(0, Ordering::Relaxed);
                    return Err(app_code_to_rmcp(&code, &message));
                }
                Err(SocketErr::Transport(reason)) => {
                    let strikes = self.note_transport_strike(&reason);
                    tracing::debug!(
                        op = req.op(),
                        strikes,
                        reason = %reason,
                        "contextd socket call failed; serving this call direct"
                    );
                    // fall through to Direct for THIS call
                }
            }
        }
        self.direct(state, req).await
    }

    /// Record one transport failure; latch the route to Direct (once) when the
    /// consecutive-strike count reaches `breaker_n`. Returns the strike count.
    fn note_transport_strike(&self, reason: &str) -> usize {
        let strikes = self.error_count.fetch_add(1, Ordering::Relaxed) + 1;
        if strikes >= self.breaker_n {
            self.route.store(ROUTE_DIRECT, Ordering::Relaxed);
            self.fallback_logged.call_once(|| {
                tracing::warn!(
                    strikes,
                    reason,
                    "lunaris-contextd unreachable; latching mcp to direct-open fallback \
                     for this session"
                );
            });
        }
        strikes
    }

    /// One connection-per-call round trip to contextd.
    ///
    /// Windows note: contextd speaks a unix socket, so the socket leg only
    /// exists on unix targets; the `#[cfg(not(unix))]` twin below keeps
    /// `dispatch` compiling (and Direct-only, see `new()`) everywhere else.
    #[cfg(unix)]
    async fn try_socket(&self, req: &MemoryRequest) -> Result<Value, SocketErr> {
        let path = self
            .socket_path
            .as_ref()
            .ok_or_else(|| SocketErr::Transport("no contextd socket configured".to_owned()))?;
        // MUST frame as `{"type":"memory", ...}` — contextd decodes a
        // `ContextRequest` (internally tagged on `type`) and rejects a bare
        // MemoryRequest ("missing field `type`"). See encode_socket_request.
        let bytes = lunaris_memory_service::protocol::encode_socket_request(req)
            .map_err(|e| SocketErr::Transport(format!("encode: {e}")))?;

        let mut stream =
            match tokio::time::timeout(self.connect_timeout, UnixStream::connect(path)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => return Err(SocketErr::Transport(format!("connect: {e}"))),
                Err(_) => {
                    return Err(SocketErr::Transport(format!(
                        "connect timed out after {:?}",
                        self.connect_timeout
                    )));
                }
            };

        stream.write_all(&bytes).await.map_err(|e| SocketErr::Transport(format!("write: {e}")))?;
        // Half-close the write side so the daemon's read_to_end completes.
        stream.shutdown().await.map_err(|e| SocketErr::Transport(format!("shutdown: {e}")))?;

        let mut resp = Vec::new();
        stream
            .read_to_end(&mut resp)
            .await
            .map_err(|e| SocketErr::Transport(format!("read: {e}")))?;
        if resp.is_empty() {
            // A daemon too old to know the Memory variant fails to parse the
            // request and closes without writing → EOF. Treat as a version/
            // transport mismatch and fall back.
            return Err(SocketErr::Transport(
                "empty response (contextd too old or refused the request)".to_owned(),
            ));
        }

        match serde_json::from_slice::<MemoryResponse>(&resp)
            .map_err(|e| SocketErr::Transport(format!("decode: {e}")))?
        {
            MemoryResponse::Ok { data } => Ok(data),
            MemoryResponse::Err { code, message } => Err(SocketErr::App { code, message }),
        }
    }

    /// Non-unix twin of `try_socket`: unreachable in practice (`new()` builds
    /// Direct-only off-unix so the route never starts at Socket), but it must
    /// exist for `dispatch` to compile on Windows targets.
    #[cfg(not(unix))]
    async fn try_socket(&self, _req: &MemoryRequest) -> Result<Value, SocketErr> {
        Err(SocketErr::Transport(
            "contextd unix socket is not supported on this platform".to_owned(),
        ))
    }

    /// Direct-open fallback — the identical shared dispatch the daemon runs,
    /// against this server's own bound engine + scope.
    async fn direct(&self, state: &AppState, req: MemoryRequest) -> Result<Value, rmcp::ErrorData> {
        // Staging is a direct-path concern: the mcp engine's embedder GGUF must
        // be on disk before a direct recall. (The socket path only reaches here
        // for recall when the daemon is down.)
        if req.needs_embedder() {
            crate::tools::staging::maybe_ensure_staged().await.map_err(rmcp::ErrorData::from)?;
        }
        lunaris_memory_service::protocol::dispatch(&state.lunaris, &state.scope, req)
            .await
            .map_err(crate::map_service_error)
    }
}

impl Default for MemoryProxy {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether this build can speak to contextd at all — the ONE place the
/// platform question is asked, so every call site stays cfg-free.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SocketSupport {
    /// Socket leg enabled at this path.
    Enabled(PathBuf),
    /// No socket leg, and nothing to say about it: operator opt-out, or no
    /// path could be derived (no `$HOME`), or a non-unix host that never
    /// asked for contextd.
    Disabled,
    /// The operator explicitly configured a socket, but this target has no
    /// unix domain sockets (Windows). Reported once — see
    /// [`SocketSupport::into_socket_path`] — then Direct-only.
    UnsupportedPlatform { configured: PathBuf },
}

impl SocketSupport {
    /// Collapse to "which socket path, if any", warning once on the
    /// unsupported-platform outcome. The warn lives here rather than at the
    /// call site so the diagnostic cannot be dropped by a future caller: a
    /// silent downgrade to Direct-only looks identical to a healthy
    /// Direct-only proxy, while costing a second resident model copy per
    /// session (the daemon is the intended single inference host).
    fn into_socket_path(self) -> Option<PathBuf> {
        match self {
            Self::Enabled(path) => Some(path),
            Self::Disabled => None,
            Self::UnsupportedPlatform { configured } => {
                tracing::warn!(
                    socket = %configured.display(),
                    env = CONTEXTD_SOCKET_ENV,
                    "contextd socket configured, but this platform has no unix domain \
                     sockets; serving every memory op direct (in-process engine)"
                );
                None
            }
        }
    }
}

/// Resolve the contextd socket for this platform, honoring the operator
/// escape hatch.
///
/// Pure by construction: platform, env and `$HOME` all arrive as parameters
/// so the non-unix branch is executable on the unix hosts where the tests
/// actually run. A `#[cfg(not(unix))]` twin would be dead text everywhere
/// except a Windows runner — and the msvc target cannot be cross-checked
/// from macOS/Linux (the `ring` / `llama-cpp-sys-2` / `aws-lc-sys` build
/// scripts need Windows C/C++ headers).
fn resolve_socket_support(
    unix_sockets: bool,
    disabled: bool,
    explicit: Option<PathBuf>,
    home: Option<PathBuf>,
) -> SocketSupport {
    // The opt-out wins on every platform, and is never worth a warning.
    if disabled {
        return SocketSupport::Disabled;
    }
    match (explicit, unix_sockets) {
        (Some(configured), false) => SocketSupport::UnsupportedPlatform { configured },
        (Some(path), true) => SocketSupport::Enabled(path),
        // Nothing configured on a socket-less target: quietly Direct-only.
        (None, false) => SocketSupport::Disabled,
        // Mirrors lunaris-contextd's default (~/.lunaris/codex-contextd.sock).
        (None, true) => home.map_or(SocketSupport::Disabled, |home| {
            SocketSupport::Enabled(home.join(".lunaris").join("codex-contextd.sock"))
        }),
    }
}

/// Map a daemon-supplied error `code` back to an rmcp wire error. Caller faults
/// are `invalid_params`; everything else is an internal error.
fn app_code_to_rmcp(code: &str, message: &str) -> rmcp::ErrorData {
    match code {
        "invalid_input" | "scope_required" => {
            rmcp::ErrorData::invalid_params(message.to_owned(), None)
        }
        _ => rmcp::ErrorData::internal_error(message.to_owned(), None),
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use lunaris_core::{Scope, StubEmbedder};
    use lunaris_memory_service::ingest::IngestParams;
    use lunaris_memory_service::recall::RecallParams;

    use crate::state::AppState;
    use lunaris_test_harness::{TestStore, open_test_engine_with_embedder};

    /// Deterministic constructor (no env / fs) for state-machine tests.
    fn proxy_for_test(breaker_n: usize, initial_route: u8) -> MemoryProxy {
        proxy_for_test_with_socket(breaker_n, initial_route, None)
    }

    /// As [`proxy_for_test`] but with an explicit socket path (e.g. a
    /// nonexistent one to exercise the connect-refused fallback).
    fn proxy_for_test_with_socket(
        breaker_n: usize,
        initial_route: u8,
        socket_path: Option<PathBuf>,
    ) -> MemoryProxy {
        MemoryProxy {
            socket_path,
            route: AtomicU8::new(initial_route),
            error_count: AtomicUsize::new(0),
            // Keep the connect budget tiny so a dead-socket test does not wait.
            connect_timeout: Duration::from_millis(100),
            breaker_n,
            fallback_logged: Once::new(),
        }
    }

    /// In-process AppState (harness-issued ephemeral Moon + StubEmbedder, no
    /// GGUF) for the direct fallback path.
    ///
    /// 0.7.0 port off `memory://`. `AppState` holds an `Arc<Lunaris>`, so the
    /// deref-transparent `TestEngine` is split via `into_parts()`; the returned
    /// [`TestStore`] owns the Moon child and must stay bound for the test.
    ///
    /// This is the harness, NOT the `embedded-moon` feature — no test binary in
    /// this crate links the Moon server (CLAUDE.md invariant).
    async fn fresh_state(scope_name: &str) -> (AppState, TestStore) {
        let embedder = Arc::new(StubEmbedder::new(768));
        let (lunaris, store) = open_test_engine_with_embedder(embedder).await.into_parts();
        let scope = Scope::new(scope_name).unwrap();
        let state = AppState {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None,
        };
        (state, store)
    }

    #[test]
    fn breaker_latches_to_direct_after_exactly_n_strikes() {
        let proxy = proxy_for_test(3, ROUTE_SOCKET);
        assert_eq!(proxy.route.load(Ordering::Relaxed), ROUTE_SOCKET);

        assert_eq!(proxy.note_transport_strike("connect: refused"), 1);
        assert_eq!(proxy.route.load(Ordering::Relaxed), ROUTE_SOCKET, "1 strike must not latch");

        assert_eq!(proxy.note_transport_strike("connect: refused"), 2);
        assert_eq!(proxy.route.load(Ordering::Relaxed), ROUTE_SOCKET, "2 strikes must not latch");

        assert_eq!(proxy.note_transport_strike("connect: refused"), 3);
        assert_eq!(
            proxy.route.load(Ordering::Relaxed),
            ROUTE_DIRECT,
            "the Nth strike must latch the route to Direct"
        );
    }

    #[test]
    fn healthy_reply_resets_the_strike_count() {
        let proxy = proxy_for_test(3, ROUTE_SOCKET);
        proxy.note_transport_strike("io");
        proxy.note_transport_strike("io");
        // A healthy socket reply resets the counter (as dispatch does on Ok).
        proxy.error_count.store(0, Ordering::Relaxed);
        // One more strike after reset must be counted as the first, not the 3rd.
        assert_eq!(proxy.note_transport_strike("io"), 1);
        assert_eq!(
            proxy.route.load(Ordering::Relaxed),
            ROUTE_SOCKET,
            "reset must prevent a stale strike count from tripping the breaker"
        );
    }

    #[test]
    fn breaker_of_one_latches_on_first_strike() {
        let proxy = proxy_for_test(1, ROUTE_SOCKET);
        proxy.note_transport_strike("connect");
        assert_eq!(proxy.route.load(Ordering::Relaxed), ROUTE_DIRECT);
    }

    #[test]
    fn app_error_codes_map_to_the_right_rmcp_class() {
        // Caller faults → invalid_params (-32602); everything else → internal.
        let invalid = app_code_to_rmcp("invalid_input", "bad ulid");
        assert_eq!(invalid.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let scope = app_code_to_rmcp("scope_required", "empty scope");
        assert_eq!(scope.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let engine = app_code_to_rmcp("engine_error", "moon down");
        assert_eq!(engine.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        let unknown = app_code_to_rmcp("unknown_index", "index missing");
        assert_eq!(unknown.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    // ── Batch 4: failure-model + parity ───────────────────────────────────────

    /// contextd-DOWN: a Socket-first proxy whose socket does not exist must fall
    /// back to the direct engine and still return a valid DTO — and latch the
    /// route to Direct. This is the "mcp keeps working when the daemon is gone"
    /// guarantee at the heart of the fallback design.
    #[tokio::test]
    async fn contextd_down_falls_back_to_direct_and_serves_the_call() {
        let (state, _store) = fresh_state("test-proxy-fallback").await;
        // A nonexistent socket → connect refused → transport strike → Direct.
        let dead = PathBuf::from("/nonexistent/lunaris-contextd-does-not-exist.sock");
        let proxy = proxy_for_test_with_socket(1, ROUTE_SOCKET, Some(dead));

        let req = MemoryRequest::Ingest {
            scope: state.scope.as_str().to_owned(),
            params: IngestParams {
                source: "test/src".to_owned(),
                content: "fallback works".to_owned(),
                t_ref: None,
                metadata: None,
                dedupe_key: None,
            },
        };
        let data = proxy.dispatch(&state, req).await.expect("direct fallback must serve the call");
        assert!(data.get("lsn").and_then(|l| l.as_str()).is_some(), "ingest DTO must carry lsn");
        assert_eq!(
            proxy.route.load(Ordering::Relaxed),
            ROUTE_DIRECT,
            "a dead socket must latch the route to Direct"
        );
    }

    /// PARITY: the proxy's direct fallback returns byte-identical JSON to a bare
    /// `protocol::dispatch` on the same engine. Because contextd's socket path
    /// ALSO calls `protocol::dispatch`, this proves the two surfaces cannot
    /// diverge — the safety rule this whole task exists to enforce.
    #[tokio::test]
    async fn direct_fallback_is_byte_identical_to_bare_dispatch() {
        // Recall touches the embedder on the direct path — skip the GGUF stage.
        crate::tools::staging::skip_stage_for_tests();
        let (state, _store) = fresh_state("test-proxy-parity").await;

        // Seed one episode through the proxy (socket down → direct).
        let dead = PathBuf::from("/nonexistent/parity.sock");
        let proxy = proxy_for_test_with_socket(1, ROUTE_SOCKET, Some(dead));
        let ingest = MemoryRequest::Ingest {
            scope: state.scope.as_str().to_owned(),
            params: IngestParams {
                source: "test/src".to_owned(),
                content: "chocolate cake recipe with cocoa".to_owned(),
                t_ref: None,
                metadata: None,
                dedupe_key: None,
            },
        };
        proxy.dispatch(&state, ingest).await.expect("seed ingest");

        let recall_params = || RecallParams {
            query: "chocolate".to_owned(),
            k: 5,
            filters: None,
            as_of: None,
            raw: false,
        };
        // (a) via the proxy's direct fallback.
        let via_proxy = proxy
            .dispatch(
                &state,
                MemoryRequest::Recall {
                    scope: state.scope.as_str().to_owned(),
                    params: recall_params(),
                },
            )
            .await
            .expect("proxy recall");
        // (b) via the bare shared dispatch — the exact fn contextd's socket path runs.
        let via_dispatch = lunaris_memory_service::protocol::dispatch(
            &state.lunaris,
            &state.scope,
            MemoryRequest::Recall {
                scope: state.scope.as_str().to_owned(),
                params: recall_params(),
            },
        )
        .await
        .expect("bare dispatch recall");

        assert_eq!(
            via_proxy, via_dispatch,
            "proxy direct fallback must be byte-identical to bare protocol::dispatch"
        );
        let hits = via_proxy.get("hits").and_then(|h| h.as_array()).expect("hits");
        assert!(!hits.is_empty(), "parity recall must find the seeded episode");
    }

    /// Unified-inference contract (2026-07-19): the mcp engine has no resident
    /// embedder — an embed-needing op served Direct forces a full local weight
    /// load. So `needs_embedder()` requests must attempt the contextd socket
    /// on EVERY call, even after the breaker latched the route to Direct: one
    /// cheap connect keeps contextd the single inference host the moment the
    /// daemon recovers. Embed-free ops keep the latched fast path untouched.
    #[tokio::test]
    async fn embed_ops_attempt_socket_even_when_latched_direct() {
        crate::tools::staging::skip_stage_for_tests();
        let (state, _store) = fresh_state("test-proxy-embed-retry").await;
        let dead = PathBuf::from("/nonexistent/embed-retry.sock");
        let proxy = proxy_for_test_with_socket(usize::MAX, ROUTE_DIRECT, Some(dead));

        // Embed-free op (Ingest) on a latched-Direct route: no socket attempt.
        let ingest = MemoryRequest::Ingest {
            scope: state.scope.as_str().to_owned(),
            params: IngestParams {
                source: "test/src".to_owned(),
                content: "embed retry seed".to_owned(),
                t_ref: None,
                metadata: None,
                dedupe_key: None,
            },
        };
        proxy.dispatch(&state, ingest).await.expect("latched ingest must serve direct");
        assert_eq!(
            proxy.error_count.load(Ordering::Relaxed),
            0,
            "an embed-free op on a latched route must NOT attempt the socket"
        );

        // Embed op (Recall) on the same latched route: the dead socket MUST be
        // attempted (one transport strike recorded), then served Direct anyway.
        let recall = MemoryRequest::Recall {
            scope: state.scope.as_str().to_owned(),
            params: RecallParams {
                query: "embed retry".to_owned(),
                k: 3,
                filters: None,
                as_of: None,
                raw: false,
            },
        };
        proxy.dispatch(&state, recall).await.expect("latched recall must still serve direct");
        assert_eq!(
            proxy.error_count.load(Ordering::Relaxed),
            1,
            "an embed-needing op must attempt the contextd socket even after the \
             breaker latched Direct (unified-inference contract)"
        );
    }

    // ── Split-routing containment (task #20) ─────────────────────────────────

    /// Spawn a one-shot fake contextd: accepts EXACTLY one connection, answers
    /// with the supplied JSON, then unlinks the socket so every later connect
    /// is refused (a daemon that died mid-session).
    #[cfg(unix)]
    fn spawn_one_shot_contextd(
        dir: &std::path::Path,
        reply: serde_json::Value,
    ) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let path = dir.join("contextd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind fake contextd");
        listener.set_nonblocking(false).unwrap();
        let sock = path.clone();
        let handle = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = Vec::new();
                let _ = stream.read_to_end(&mut buf);
                let _ = stream.write_all(reply.to_string().as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
            // Daemon dies: drop the listener AND unlink, so the next connect
            // is a transport failure rather than a hang.
            drop(listener);
            let _ = std::fs::remove_file(&sock);
        });
        (path, handle)
    }

    /// SPLIT-ROUTING (task #20). The socket route and the Direct route derive
    /// their store from two INDEPENDENT sources that nothing reconciles:
    /// contextd resolves `LUNARIS_STORE_URL` / `~/.lunaris/contextd-moon.url`
    /// in ITS OWN process env (`lunaris-hook/src/context.rs:649,670`), while
    /// the mcp engine is opened once at boot from `--storage` /
    /// `LUNARIS_MCP_STORAGE` (`state.rs:162`).
    ///
    /// So when contextd answers from Moon A and then dies, the breaker latches
    /// this session to Direct and the SAME logical op stream continues against
    /// Moon B — an ingest served over the socket is invisible to the forget
    /// served Direct. `direct_fallback_is_byte_identical_to_bare_dispatch`
    /// proves the two routes run identical CODE; it says nothing about them
    /// running against the same STORE.
    ///
    /// Contract: once contextd has reported a store that differs from this
    /// server's own, the Direct fallback is REFUSED — a named error the
    /// operator can act on, never a silent write into the second store.
    #[cfg(unix)]
    #[tokio::test]
    async fn direct_fallback_is_refused_when_contextd_reported_a_different_store() {
        let (state, store) = fresh_state("test-proxy-split-store").await;
        let tmp = tempfile::tempdir().unwrap();

        // contextd answers one call, reporting a store that is NOT ours.
        let foreign = "moon://127.0.0.1:1";
        assert_ne!(foreign, store.url(), "the fake daemon must name a different store");
        let (sock, server) = spawn_one_shot_contextd(
            tmp.path(),
            serde_json::json!({"status": "ok", "data": {"lsn": "0-1"}, "store": foreign}),
        );
        let proxy = proxy_for_test_with_socket(1, ROUTE_SOCKET, Some(sock));

        let ingest = |content: &str| MemoryRequest::Ingest {
            scope: state.scope.as_str().to_owned(),
            params: IngestParams {
                source: "test/src".to_owned(),
                content: content.to_owned(),
                t_ref: None,
                metadata: None,
                dedupe_key: None,
            },
        };

        // Call 1 — served over the socket by the (foreign-store) daemon.
        let first = proxy.dispatch(&state, ingest("written to moon A")).await;
        assert!(first.is_ok(), "the socket call must succeed: {first:?}");
        server.await.unwrap();

        // Call 2 — daemon is gone. Serving this Direct would write into OUR
        // store while call 1 landed in the daemon's: the split this test pins.
        let second = proxy.dispatch(&state, ingest("must NOT land in moon B")).await;
        let err = second.expect_err(
            "with contextd's store known to differ, the Direct fallback MUST be refused — \
             serving it splits one op stream across two Moons",
        );
        let msg = format!("{err:?}");
        for needle in ["store", foreign] {
            assert!(msg.contains(needle), "refusal must name {needle}: {msg}");
        }
    }

    /// The fallback must stay OPEN in the normal case: a daemon that reports
    /// the SAME store (or an older daemon that reports none at all) is not a
    /// split, and refusing there would break every standalone install.
    #[cfg(unix)]
    #[tokio::test]
    async fn direct_fallback_is_allowed_when_contextd_reported_the_same_store() {
        let (state, store) = fresh_state("test-proxy-same-store").await;
        let tmp = tempfile::tempdir().unwrap();
        let (sock, server) = spawn_one_shot_contextd(
            tmp.path(),
            serde_json::json!({"status": "ok", "data": {"lsn": "0-1"}, "store": store.url()}),
        );
        let proxy = proxy_for_test_with_socket(1, ROUTE_SOCKET, Some(sock));

        let ingest = |content: &str| MemoryRequest::Ingest {
            scope: state.scope.as_str().to_owned(),
            params: IngestParams {
                source: "test/src".to_owned(),
                content: content.to_owned(),
                t_ref: None,
                metadata: None,
                dedupe_key: None,
            },
        };

        proxy.dispatch(&state, ingest("same store")).await.expect("socket call");
        server.await.unwrap();

        let second = proxy
            .dispatch(&state, ingest("fallback is safe here"))
            .await
            .expect("same-store fallback must still be served Direct");
        assert!(second.get("lsn").is_some(), "direct fallback must return a real ingest DTO");
    }

    /// T-25-01-01: the external MCP wire cannot inject a partition `scope`. The
    /// engine Params DTOs carry `#[serde(deny_unknown_fields)]` and no scope
    /// field, so a smuggled `scope` is rejected at deserialization — the scope
    /// is bound at startup and threaded server-side only (via MemoryRequest,
    /// which the client never fills from the wire).
    #[test]
    fn wire_payload_cannot_smuggle_a_scope_field() {
        let smuggled = serde_json::json!({
            "source": "attacker",
            "content": "payload",
            "scope": "victim-scope"
        });
        let parsed = serde_json::from_value::<IngestParams>(smuggled);
        assert!(parsed.is_err(), "deny_unknown_fields must reject a wire-injected scope");

        let smuggled_recall = serde_json::json!({ "query": "q", "scope": "victim-scope" });
        assert!(
            serde_json::from_value::<RecallParams>(smuggled_recall).is_err(),
            "recall DTO must also reject a wire-injected scope"
        );
    }

    // ── Platform seam (Windows) ────────────────────────────────────────────
    //
    // The socket leg is unix-only. The decision "does this build have unix
    // domain sockets, and did the operator ask for one anyway?" must be a
    // PURE function of (platform, env, home) so a unix dev box / unix CI can
    // execute the non-unix branch. `cfg!(unix)` is passed in as a parameter
    // for exactly that reason — with `#[cfg(not(unix))]` the branch would be
    // untestable everywhere the tests actually run.

    const SOCK_ENV: &str = "/run/user/1000/lunaris-contextd.sock";

    fn home() -> Option<PathBuf> {
        Some(PathBuf::from("/home/dev"))
    }

    #[test]
    fn unix_host_with_explicit_socket_enables_the_socket_leg() {
        let got = resolve_socket_support(true, false, Some(PathBuf::from(SOCK_ENV)), home());
        assert_eq!(got, SocketSupport::Enabled(PathBuf::from(SOCK_ENV)));
    }

    #[test]
    fn unix_host_without_explicit_socket_falls_back_to_the_home_default() {
        let got = resolve_socket_support(true, false, None, home());
        assert_eq!(
            got,
            SocketSupport::Enabled(PathBuf::from("/home/dev/.lunaris/codex-contextd.sock"))
        );
    }

    /// THE Windows contract: an operator who explicitly configured
    /// `LUNARIS_CONTEXTD_SOCKET` on a target without unix domain sockets must
    /// be told — silently degrading to Direct hides a misconfiguration that
    /// costs a second resident model copy per session.
    #[test]
    fn non_unix_host_with_explicit_socket_is_reported_not_silently_dropped() {
        let got = resolve_socket_support(false, false, Some(PathBuf::from(SOCK_ENV)), home());
        assert_eq!(
            got,
            SocketSupport::UnsupportedPlatform { configured: PathBuf::from(SOCK_ENV) },
            "an explicitly configured socket on a non-unix target must surface, not vanish"
        );
    }

    /// Nothing was configured, so there is nothing to warn about: a Windows
    /// user who never asked for contextd gets a quiet Direct-only proxy.
    #[test]
    fn non_unix_host_without_explicit_socket_is_quietly_direct_only() {
        assert_eq!(resolve_socket_support(false, false, None, home()), SocketSupport::Disabled);
    }

    /// The operator opt-out wins on every platform, and never warns.
    #[test]
    fn disable_env_wins_over_platform_and_explicit_socket() {
        assert_eq!(
            resolve_socket_support(true, true, Some(PathBuf::from(SOCK_ENV)), home()),
            SocketSupport::Disabled
        );
        assert_eq!(
            resolve_socket_support(false, true, Some(PathBuf::from(SOCK_ENV)), home()),
            SocketSupport::Disabled
        );
    }

    /// No `$HOME` and no explicit socket → no path can be derived.
    #[test]
    fn missing_home_disables_the_socket_leg() {
        assert_eq!(resolve_socket_support(true, false, None, None), SocketSupport::Disabled);
    }

    /// `SocketSupport` is the ONLY place the platform question is asked, so
    /// every outcome must collapse into "which socket path, if any" for the
    /// cfg-free call sites in `dispatch`.
    #[test]
    fn unsupported_platform_yields_no_socket_path() {
        let support = resolve_socket_support(false, false, Some(PathBuf::from(SOCK_ENV)), home());
        assert!(
            support.into_socket_path().is_none(),
            "a non-unix host must never hand a socket path to the proxy"
        );
    }
}
