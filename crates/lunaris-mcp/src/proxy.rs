//! Socket-first proxy to `lunaris-contextd` with direct-open fallback.
//!
//! contextd-mcp-merge batch 3. Each engine `#[tool]` builds a
//! [`MemoryRequest`] and hands it here. The proxy is Socket-first: it forwards
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        let socket_path = resolve_socket_path();
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

    /// Route one engine op. Socket-first with direct-open fallback.
    pub(crate) async fn dispatch(
        &self,
        state: &AppState,
        req: MemoryRequest,
    ) -> Result<Value, rmcp::ErrorData> {
        if self.route.load(Ordering::Relaxed) == ROUTE_SOCKET {
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
    async fn try_socket(&self, req: &MemoryRequest) -> Result<Value, SocketErr> {
        let path = self
            .socket_path
            .as_ref()
            .ok_or_else(|| SocketErr::Transport("no contextd socket configured".to_owned()))?;
        let bytes =
            serde_json::to_vec(req).map_err(|e| SocketErr::Transport(format!("encode: {e}")))?;

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

/// Resolve the contextd socket path, honoring the operator escape hatch.
fn resolve_socket_path() -> Option<PathBuf> {
    // Force Direct-only (tests / air-gapped operators / no daemon wanted).
    if std::env::var_os("LUNARIS_MCP_DISABLE_CONTEXTD").is_some() {
        return None;
    }
    if let Some(p) = std::env::var_os(CONTEXTD_SOCKET_ENV) {
        return Some(PathBuf::from(p));
    }
    // Mirrors lunaris-contextd's default (~/.lunaris/codex-contextd.sock).
    dirs::home_dir().map(|home| home.join(".lunaris").join("codex-contextd.sock"))
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

    /// Deterministic constructor (no env / fs) for state-machine tests.
    fn proxy_for_test(breaker_n: usize, initial_route: u8) -> MemoryProxy {
        MemoryProxy {
            socket_path: None,
            route: AtomicU8::new(initial_route),
            error_count: AtomicUsize::new(0),
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_MS),
            breaker_n,
            fallback_logged: Once::new(),
        }
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
}
