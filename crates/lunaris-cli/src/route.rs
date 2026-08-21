//! Socket-first routing to `lunaris-contextd`, with a direct-open fallback.
//!
//! Deliberately the same shape as `lunaris-mcp/src/proxy.rs`, because the
//! alternative — inventing a second routing policy — is how surfaces drift
//! apart. The rules that matter:
//!
//! - **Socket first.** A running contextd already holds the store handle and
//!   the loaded models; going through it avoids a cold start and keeps one
//!   process authoritative for a scope.
//! - **Fall back on TRANSPORT failure only.** An application error returned
//!   *by* the daemon is authoritative: the daemon reached the engine, so
//!   retrying the same request directly against the same storage would only
//!   repeat it.
//! - **One-shot, so no circuit breaker.** The MCP proxy latches to Direct
//!   after N strikes because it is long-lived and serves many calls. A CLI
//!   invocation makes exactly one request, so a strike counter would have
//!   nothing to count; the connect timeout is the whole budget.
//!
//! What this does NOT do is insert itself between the hook or the MCP server
//! and contextd. Those already reach `dispatch` in-process; routing them
//! through a CLI would add a process spawn to a hot path in exchange for a
//! consistency guarantee the shared dispatch already provides.

use std::path::PathBuf;
use std::time::Duration;

use lunaris_memory_service::protocol::{MemoryRequest, MemoryResponse};
use serde_json::Value;

/// Cold-start connect budget. A missing or wedged daemon must not stall the
/// command past this before the direct path takes over.
const DEFAULT_CONNECT_MS: u64 = 500;

/// Which path answered. Printed under `--json` so a script can tell whether it
/// read through the warm daemon or opened the store itself — the two can, in
/// principle, be pointed at different Moons, and silently not knowing which
/// one answered is the bug PR #118 fixed for MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Socket,
    Direct,
    /// `lunaris try`: an in-process Moon this command started and will shut
    /// down. A distinct label because "direct" would be actively misleading —
    /// nothing about a trial store persists, and a reader who confuses it with
    /// their real store will wonder where their memories went.
    Trial,
}

impl Route {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Route::Socket => "contextd",
            Route::Direct => "direct",
            Route::Trial => "lunaris try — embedded Moon",
        }
    }
}

/// Failure of the socket leg, split by whose fault it is.
enum SocketErr {
    /// Could not talk to the daemon. Falling back is correct.
    Transport(String),
    /// The daemon answered with an engine error. Authoritative — do NOT retry
    /// directly; a direct attempt hits the same storage and fails identically,
    /// while hiding the fact that the daemon was reachable.
    App { code: String, message: String },
}

pub(crate) struct Router {
    socket_path: Option<PathBuf>,
    connect_timeout: Duration,
}

impl Router {
    pub(crate) fn new(socket_path: Option<PathBuf>) -> Self {
        let ms = std::env::var("LUNARIS_CLI_CONNECT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CONNECT_MS);
        Self { socket_path, connect_timeout: Duration::from_millis(ms) }
    }

    /// Route one request. Returns the response payload and which leg served it.
    pub(crate) async fn dispatch(&self, req: MemoryRequest) -> anyhow::Result<(Value, Route)> {
        #[cfg(unix)]
        if self.socket_path.is_some() {
            match self.try_socket(&req).await {
                Ok(value) => return Ok((value, Route::Socket)),
                Err(SocketErr::App { code, message }) => {
                    anyhow::bail!("contextd returned {code}: {message}");
                }
                Err(SocketErr::Transport(why)) => {
                    tracing::debug!(%why, "contextd unreachable; serving this call direct");
                }
            }
        }

        let value = crate::direct::dispatch_direct(req).await?;
        Ok((value, Route::Direct))
    }

    #[cfg(unix)]
    async fn try_socket(&self, req: &MemoryRequest) -> Result<Value, SocketErr> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        let path = self
            .socket_path
            .as_ref()
            .ok_or_else(|| SocketErr::Transport("no contextd socket configured".to_owned()))?;

        // MUST frame as `{"type":"memory", ...}`: contextd decodes a
        // `ContextRequest` (internally tagged on `type`) and rejects a bare
        // MemoryRequest with "missing field `type`".
        let bytes = lunaris_memory_service::protocol::encode_socket_request(req)
            .map_err(|e| SocketErr::Transport(format!("encode: {e}")))?;

        let mut stream =
            match tokio::time::timeout(self.connect_timeout, UnixStream::connect(path)).await {
                Ok(Ok(s)) => s,
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
            // A daemon too old to know the Memory variant fails to parse and
            // closes without writing → EOF. Version/transport mismatch.
            return Err(SocketErr::Transport(
                "empty response (contextd too old or refused the request)".to_owned(),
            ));
        }

        match serde_json::from_slice::<MemoryResponse>(&resp)
            .map_err(|e| SocketErr::Transport(format!("decode: {e}")))?
        {
            MemoryResponse::Ok { data, .. } => Ok(data),
            MemoryResponse::Err { code, message } => Err(SocketErr::App { code, message }),
        }
    }
}
