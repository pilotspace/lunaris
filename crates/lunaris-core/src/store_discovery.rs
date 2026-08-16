//! Contextd store discovery — shared by `lunaris-hook` and `lunaris-mcp`.
//!
//! `lunaris-contextd` writes `~/.lunaris/contextd-moon.url` when its embedded
//! Moon is ready. Every Lunaris binary that needs a store reads that file and
//! **liveness-probes** the endpoint before trusting it. This module owns that
//! one implementation so the two binaries cannot drift into disagreeing about
//! what "the advertised store" means — a disagreement that shows up as split
//! routing (an ingest one process can see and the other cannot), which is the
//! exact failure `lunaris-mcp`'s proxy now refuses to serve through.
//!
//! # Why it lives in `lunaris-core`
//!
//! `lunaris-hook` and `lunaris-mcp` deliberately do NOT depend on each other,
//! and `lunaris-core::scope_resolver` is already the shared home for the other
//! half of this bootstrap (both binaries derive identical scopes through it).
//! The error this discovery declines into — [`ScopeResolveError::NoStoreUrl`] —
//! is defined there too. This module is `std`-only: no new dependency, and no
//! engine/transport crate is dragged into a decision made before either exists.
//!
//! # Read once, at boot
//!
//! Callers resolve at startup and keep the URL for the process's lifetime. A
//! discovery file that appears (or changes) mid-flight is NOT picked up — a
//! long-lived server silently re-pointing its store mid-session would be a
//! worse failure than the restart it saves. Restart the binary after starting
//! `lunaris-contextd`.

use std::path::Path;

/// Discovery-file name under `~/.lunaris`. Written by `lunaris-contextd` when
/// its embedded Moon is ready (feature `embedded-moon`); read by EVERY
/// resolver, feature or not — a missing file simply falls through.
pub const CONTEXTD_MOON_URL_FILE: &str = "contextd-moon.url";

/// Env var overriding the discovery liveness-probe budget, in milliseconds.
///
/// Default [`DEFAULT_PROBE_TIMEOUT_MS`]. `0` errors the connect — i.e. skips
/// discovery entirely, a documented escape hatch rather than a crash
/// (`connect_timeout` returns `InvalidInput` for a zero duration).
pub const PROBE_TIMEOUT_ENV: &str = "LUNARIS_MOON_DISCOVERY_TIMEOUT_MS";

/// Default liveness-probe budget. Small on purpose: this runs on the startup
/// path of a one-shot hook binary, where a stale file must cost milliseconds.
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 25;

/// Outcome of reading + probing the contextd discovery file.
///
/// The three arms exist so a caller can tell an operator *why* it is refusing:
/// "nothing advertised a store" and "something advertised a store that is not
/// answering" are different problems with different fixes, and collapsing them
/// into `Option<String>` costs exactly the sentence the operator needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreDiscovery {
    /// A loopback endpoint was advertised AND answered a RESP `PING`.
    Live(String),
    /// A discovery file exists but is not trustworthy: stale (nobody home),
    /// hijacked (something answers TCP but not RESP), tampered (non-loopback),
    /// unparseable, or unreadable. Never fails open.
    Declined,
    /// No discovery file at all — `lunaris-contextd` is not running, or was
    /// built without `embedded-moon`.
    Absent,
}

impl StoreDiscovery {
    /// The advertised URL, if it passed the probe.
    #[must_use]
    pub fn into_url(self) -> Option<String> {
        match self {
            Self::Live(url) => Some(url),
            Self::Declined | Self::Absent => None,
        }
    }
}

/// Read `<lunaris_dir>/contextd-moon.url` and liveness-probe the advertised
/// endpoint.
///
/// Returns [`StoreDiscovery::Live`] only when:
/// - the address is LOOPBACK (contextd only ever advertises 127.0.0.1 — a
///   discovery file pointing anywhere else is treated as tampered and ignored,
///   so it can never redirect captures off-host), and
/// - the endpoint answers a real RESP `PING` with `+PONG` within the probe
///   budget ([`PROBE_TIMEOUT_ENV`], default [`DEFAULT_PROBE_TIMEOUT_MS`]).
///
/// The PING (not a bare TCP connect) matters because the discovery file is
/// never cleaned up: after contextd dies, the OS will eventually reassign its
/// ephemeral port to some unrelated process. A random listener accepts TCP but
/// does not answer `+PONG`, so the probe fails and discovery is declined. A
/// stale/garbage/unwritable file behaves the same — this function must NEVER
/// hang and never propagate somebody else's leftovers as a store.
#[must_use]
pub fn discover_contextd_moon(lunaris_dir: &Path) -> StoreDiscovery {
    discover_contextd_moon_file(&lunaris_dir.join(CONTEXTD_MOON_URL_FILE))
}

/// [`discover_contextd_moon`] against an explicit file path.
#[must_use]
pub fn discover_contextd_moon_file(url_file: &Path) -> StoreDiscovery {
    let raw = match std::fs::read_to_string(url_file) {
        Ok(raw) => raw,
        // Nothing advertised — the ordinary "contextd is not running" case.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return StoreDiscovery::Absent,
        // A file exists but cannot be read (permissions, a directory in its
        // place). That is a misconfiguration to report, not an absence.
        Err(_) => return StoreDiscovery::Declined,
    };
    let url = raw.trim();
    let Some(addr) = url.strip_prefix("moon://") else {
        return StoreDiscovery::Declined;
    };
    let Ok(sock) = addr.parse::<std::net::SocketAddr>() else {
        return StoreDiscovery::Declined;
    };
    if !sock.ip().is_loopback() {
        return StoreDiscovery::Declined;
    }
    if probe_resp_ping(sock) {
        StoreDiscovery::Live(url.to_string())
    } else {
        StoreDiscovery::Declined
    }
}

/// Connect and exchange one RESP `PING`. Any failure — connect, write, read,
/// or a reply that is not `+PONG` — is a `false`, never a propagated error.
fn probe_resp_ping(sock: std::net::SocketAddr) -> bool {
    let timeout_ms = std::env::var(PROBE_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_PROBE_TIMEOUT_MS);
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let probe = || -> std::io::Result<bool> {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect_timeout(&sock, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(b"*1\r\n$4\r\nPING\r\n")?;
        let mut buf = [0u8; 7]; // "+PONG\r\n"
        stream.read_exact(&mut buf)?;
        Ok(buf.starts_with(b"+PONG"))
    };
    matches!(probe(), Ok(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal RESP responder standing in for contextd's embedded Moon:
    /// accepts one connection and answers `+PONG` to anything.
    fn spawn_responder(reply: &'static [u8]) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 64];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(reply);
            }
        });
        (port, handle)
    }

    fn write_discovery(dir: &Path, contents: &str) {
        std::fs::write(dir.join(CONTEXTD_MOON_URL_FILE), contents).unwrap();
    }

    #[test]
    fn missing_file_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(discover_contextd_moon(tmp.path()), StoreDiscovery::Absent);
    }

    #[test]
    fn live_endpoint_resolves() {
        let (port, responder) = spawn_responder(b"+PONG\r\n");
        let tmp = tempfile::tempdir().unwrap();
        write_discovery(tmp.path(), &format!("moon://127.0.0.1:{port}\n"));
        assert_eq!(
            discover_contextd_moon(tmp.path()),
            StoreDiscovery::Live(format!("moon://127.0.0.1:{port}")),
            "a live PONG-answering endpoint is the only way discovery succeeds"
        );
        responder.join().unwrap();
    }

    #[test]
    fn tcp_accept_without_pong_is_declined() {
        // A foreign process on contextd's reused ephemeral port accepts TCP but
        // does not speak RESP — this is why the probe is a PING, not a bare
        // connect: the discovery file is never cleaned up after contextd dies.
        let (port, responder) = spawn_responder(b"HTTP/1.1 400 Bad Request\r\n");
        let tmp = tempfile::tempdir().unwrap();
        write_discovery(tmp.path(), &format!("moon://127.0.0.1:{port}\n"));
        assert_eq!(discover_contextd_moon(tmp.path()), StoreDiscovery::Declined);
        responder.join().unwrap();
    }

    #[test]
    fn non_loopback_address_is_declined() {
        // contextd only ever advertises 127.0.0.1 — a discovery file pointing
        // off-host is tampered/corrupt and must never redirect captures.
        let tmp = tempfile::tempdir().unwrap();
        write_discovery(tmp.path(), "moon://192.0.2.10:6379\n");
        assert_eq!(discover_contextd_moon(tmp.path()), StoreDiscovery::Declined);
    }

    #[test]
    fn stale_file_is_declined() {
        // Bind then DROP the listener — the advertised port is dead, exactly
        // like a discovery file left behind by a crashed contextd.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let tmp = tempfile::tempdir().unwrap();
        write_discovery(tmp.path(), &format!("moon://127.0.0.1:{port}\n"));
        assert_eq!(discover_contextd_moon(tmp.path()), StoreDiscovery::Declined);
    }

    #[test]
    fn garbage_file_is_declined() {
        let tmp = tempfile::tempdir().unwrap();
        write_discovery(tmp.path(), "redis://not-a-moon-url:abc\n");
        assert_eq!(discover_contextd_moon(tmp.path()), StoreDiscovery::Declined);
    }

    #[test]
    fn only_live_carries_a_url() {
        assert_eq!(StoreDiscovery::Declined.into_url(), None);
        assert_eq!(StoreDiscovery::Absent.into_url(), None);
        assert_eq!(
            StoreDiscovery::Live("moon://127.0.0.1:1".into()).into_url().as_deref(),
            Some("moon://127.0.0.1:1")
        );
    }
}
