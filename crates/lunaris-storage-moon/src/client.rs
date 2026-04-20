//! `MoonClient` — typed `moon-client` v0.1.0 SDK wrapped in a Lunaris-shaped handle.
//!
//! Phase 1.5 retrofit (STORE-09) replaces the previous hand-rolled `redis 0.32+` RESP
//! wrapper with the typed `moon-client` SDK at `/Users/tindang/workspaces/tind-repo/moon/sdk/rust/`.
//! The `moon-client::MoonClient` is `Clone` (cheap — backed by a shared
//! `redis::aio::MultiplexedConnection`), so we don't need an outer mutex; per-call we
//! `.clone()` the underlying client into a local `mut` binding and dispatch sub-clients
//! from there.
//!
//! ## URL grammar
//!
//! `moon://host:port[?ws=workspace]`
//!
//! Examples:
//!   * `moon://localhost:6390`               → host=localhost, port=6390, no workspace
//!   * `moon://moon.example.com:6390?ws=hot` → host=moon.example.com, port=6390, ws=hot
//!
//! The `moon://` scheme is the Lunaris-public face; internally we translate to
//! `redis://host:port` because Moon speaks the Redis wire protocol. The URL parser
//! rejects any non-`moon` scheme BEFORE any network IO so a malicious `redis://` URL
//! cannot exercise this code path (defense in depth — mirrors the URL dispatcher in
//! `crates/lunaris/src/open.rs`).
//!
//! The optional `ws` query parameter is recorded on the struct for later use (Phase 2
//! may multiplex by workspace); Plan 03 records it but does not act on it.

use lunaris_core::error::StorageError;
use moon::{MoonClient as TypedClient, MoonError};

/// Default Moon RESP port (matches Moon's `bin/moond` default).
pub const DEFAULT_MOON_PORT: u16 = 6390;

/// A live typed `moon-client` connection to a Moon instance, parsed from a `moon://` URL.
///
/// `Clone` is cheap — the underlying `moon_client::MoonClient` shares its
/// `redis::aio::MultiplexedConnection` via `Arc`. Each `clone()` yields an independent
/// handle into the same connection so concurrent requests do not contend on a single
/// mutex.
///
/// `Debug` is hand-implemented because `moon_client::MoonClient` does NOT impl `Debug`
/// in v0.1.0 (it would leak driver internals); we redact the inner connection.
#[derive(Clone)]
pub struct MoonClient {
    /// Resolved host from the `moon://host:port` URL.
    pub host: String,
    /// Resolved port; defaults to `DEFAULT_MOON_PORT` when omitted.
    pub port: u16,
    /// Optional workspace selector from the `?ws=...` query param.
    pub workspace: Option<String>,
    /// The typed `moon-client` SDK handle. Cheap to clone.
    pub(crate) inner: TypedClient,
}

impl std::fmt::Debug for MoonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoonClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("workspace", &self.workspace)
            .field("inner", &"<moon_client::MoonClient>")
            .finish()
    }
}

impl MoonClient {
    /// Parse a `moon://host:port[?ws=workspace]` URL and open a typed `moon-client`
    /// connection.
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

        // Moon speaks RESP2/RESP3 over the Redis protocol. We dial via the typed
        // moon-client SDK which internally opens a `redis::aio::MultiplexedConnection`.
        let redis_url = format!("redis://{host}:{port}");
        let inner = TypedClient::connect(redis_url.as_str()).await.map_err(moon_err)?;
        Ok(Self { host, port, workspace, inner })
    }

    /// Cheap clone of the underlying typed client. Use one clone per concurrent task.
    pub fn typed(&self) -> TypedClient {
        self.inner.clone()
    }
}

/// Map a `moon_client::MoonError` into Lunaris's `StorageError`.
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
pub(crate) fn moon_err(e: MoonError) -> StorageError {
    let s = e.to_string();
    if s.starts_with("NOSUPPORT") || s.contains("not supported") || s.contains("Unsupported") {
        StorageError::NotSupported("moon: command not supported on this server build")
    } else {
        StorageError::Backend(format!("moon: {s}"))
    }
}

/// Map a raw `redis::RedisError` into Lunaris's `StorageError`.
///
/// Used by the documented HSCAN escape hatch in `kv.rs` which calls a raw RESP
/// command directly because `moon-client` v0.1.0 does not yet expose a typed
/// wrapper for hash-scan iteration.
#[inline]
pub(crate) fn redis_err(e: redis::RedisError) -> StorageError {
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
