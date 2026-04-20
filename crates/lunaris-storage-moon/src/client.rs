//! Connection manager for Moon — wraps `redis::aio::ConnectionManager` and exposes a tiny
//! helper for raw RESP commands.
//!
//! Single connection-pool per `MoonStorage` (matches Phase 1 "no resilience" goal — Phase 2
//! adds retries / circuit breakers / per-workspace multiplexing).
//!
//! ## URL grammar
//!
//! `moon://host:port[?ws=workspace]`
//!
//! Examples:
//!   * `moon://localhost:6390`               → host=localhost, port=6390, no workspace
//!   * `moon://moon.example.com:6390?ws=hot` → host=moon.example.com, port=6390, ws=hot
//!
//! The optional `ws` query parameter is recorded on the struct for later use (Phase 2 may
//! multiplex by workspace); Plan 03 records it but does not act on it.

use lunaris_core::error::StorageError;
use redis::RedisError;
use redis::aio::ConnectionManager;

/// Default Moon RESP port (matches Moon's `bin/moond` default).
pub const DEFAULT_MOON_PORT: u16 = 6390;

/// A live RESP connection to a Moon instance, parsed from a `moon://` URL.
///
/// `Clone` is cheap — `redis::aio::ConnectionManager` shares the underlying connection
/// state via `Arc`. Each `clone()` yields an independent handle into the same connection
/// manager so concurrent requests do not contend on a single mutex.
///
/// `Debug` is hand-implemented to redact the connection state (`redis::aio::ConnectionManager`
/// does not impl `Debug` in `redis 0.32`, and printing it would leak internal driver state).
#[derive(Clone)]
pub struct MoonClient {
    /// Resolved host from the `moon://host:port` URL.
    pub host: String,
    /// Resolved port; defaults to `DEFAULT_MOON_PORT` when omitted.
    pub port: u16,
    /// Optional workspace selector from the `?ws=...` query param.
    pub workspace: Option<String>,
    conn: ConnectionManager,
}

impl std::fmt::Debug for MoonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoonClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("workspace", &self.workspace)
            .field("conn", &"<ConnectionManager>")
            .finish()
    }
}

impl MoonClient {
    /// Parse a `moon://host:port[?ws=workspace]` URL and open a `ConnectionManager`.
    ///
    /// Returns `StorageError::UnsupportedScheme` if the URL fails to parse OR if the
    /// scheme is anything other than `moon`. The unknown-scheme arm runs BEFORE any
    /// network IO so a malicious `redis://` URL cannot exercise the Moon code path
    /// (defense in depth — mirrors the URL dispatcher in `crates/lunaris/src/open.rs`).
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("moon parse: {e}")))?;
        if parsed.scheme() != "moon" {
            return Err(StorageError::UnsupportedScheme(parsed.scheme().into()));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| StorageError::Backend("moon URL missing host".into()))?
            .to_string();
        let port = parsed.port().unwrap_or(DEFAULT_MOON_PORT);
        let workspace = parsed.query_pairs().find(|(k, _)| k == "ws").map(|(_, v)| v.into_owned());

        // Moon speaks RESP2/RESP3 over the Redis protocol. We dial via the `redis` crate.
        let redis_url = format!("redis://{host}:{port}");
        let client = redis::Client::open(redis_url).map_err(redis_err)?;
        let conn = ConnectionManager::new(client).await.map_err(redis_err)?;
        Ok(Self { host, port, workspace, conn })
    }

    /// Cheap clone of the underlying `ConnectionManager`. Use one clone per task.
    pub fn conn(&self) -> ConnectionManager {
        self.conn.clone()
    }
}

/// Map a `redis::RedisError` into Lunaris's `StorageError`.
///
/// We treat any reply starting with `NOSUPPORT ` (or containing "not supported") as
/// `StorageError::NotSupported`; everything else becomes `StorageError::Backend(msg)`
/// with the raw Moon reply preserved for debugging.
///
/// ## Threat note (T-01-03-02)
///
/// Raw Moon error messages may contain internal paths or schema names. In v0 we surface
/// them as-is to internal callers. Phase 5's `lunaris-server` will scrub error strings
/// before crossing the HTTP boundary.
#[inline]
pub(crate) fn redis_err(e: RedisError) -> StorageError {
    let s = e.to_string();
    if s.starts_with("NOSUPPORT") || s.contains("not supported") {
        StorageError::NotSupported("moon: command not supported on this server build")
    } else {
        StorageError::Backend(format!("moon: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_wrong_scheme() {
        let r = MoonClient::connect("redis://localhost:6379").await;
        assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
    }

    #[tokio::test]
    async fn rejects_garbage_url() {
        let r = MoonClient::connect("not a url").await;
        assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
    }
}
