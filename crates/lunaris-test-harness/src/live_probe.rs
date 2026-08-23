//! Is the live fixture actually reachable? — one probe, for every crate.
//!
//! ## Why it lives here
//!
//! Four files under `crates/lunaris/tests/` carried a byte-similar copy of a
//! TCP probe, and all four shared one defect: they resolved the host and then
//! took `to_socket_addrs().next()` — the FIRST address — and gave up if it
//! refused. A hostname routinely resolves to more than one address, and the
//! first one is not required to be the one the server bound.
//!
//! That is not hypothetical. `.github/workflows/integration.yml` sets
//! `MOON_URL: moon://localhost:6390` and launches Moon with no `--bind`, so
//! Moon takes its default of `127.0.0.1` (`vendor/moon/src/config.rs`) —
//! IPv4 only. On a GitHub runner `localhost` resolves to `::1` as well, and
//! whichever address `getaddrinfo` returns first is the only one these probes
//! ever tried. The job's own wait-step used bash `/dev/tcp/localhost/6390`,
//! which walks the whole list, so the job saw a reachable Moon and the tests
//! did not. They skipped, and — before F27 routed them — skipped silently.
//!
//! The fix is the one every other client already implements: try EVERY address
//! the name resolves to before calling the fixture unreachable.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Per-address connect timeout. Applied to each candidate, not to the set.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Can we open a TCP connection to ANY of `addrs`?
///
/// Taking the list rather than the hostname is what makes this testable: a
/// test can hand it a dead address followed by a live one — the exact shape
/// that broke CI — on a platform whose resolver would never produce that order
/// on its own.
pub fn any_reachable(addrs: &[SocketAddr], timeout: Duration) -> bool {
    addrs.iter().any(|addr| TcpStream::connect_timeout(addr, timeout).is_ok())
}

/// Split a backend URL down to the `host:port` a probe can resolve.
///
/// Returns `None` for a scheme this harness does not speak, which the caller
/// reports as a skip rather than guessing a port for.
pub fn host_port_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next()?;
    let bare = authority.rsplit('@').next()?;
    if bare.is_empty() {
        return None;
    }
    match scheme {
        "moon" | "redis" | "rediss" => Some(bare.to_string()),
        "postgres" | "postgresql" => {
            // A Postgres URL may legitimately omit the port; Moon/Redis URLs
            // in this workspace never do.
            if bare.contains(':') { Some(bare.to_string()) } else { Some(format!("{bare}:5432")) }
        }
        _ => None,
    }
}

/// Is this URL reachable? — no announcement, no strict-mode decision.
///
/// For callers that already own their skip message (the test name belongs in
/// it) and only want the address-walking probe. Announcing here too would
/// print the same skip twice.
pub fn reachable(url: &str) -> bool {
    let Some(host_port) = host_port_of(url) else { return false };
    let addrs: Vec<SocketAddr> = match host_port.to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(_) => return false,
    };
    any_reachable(&addrs, CONNECT_TIMEOUT)
}

/// Read `env_name`, probe whatever it points at, and hand back the URL only if
/// something answered. Every failure path is announced through
/// [`crate::strict_skip`], so none of them can report success for a suite that
/// tested nothing.
pub fn probe_url_env(env_name: &str) -> Option<String> {
    probe_url_env_with(env_name, crate::strict_skip::strict())
}

/// The probe, with strictness passed in rather than read.
///
/// Same reasoning as [`crate::strict_skip::note_unavailable_with`]: a test that
/// flips the variable races every sibling in its binary. A unit test asserting
/// "an unset variable yields `None`" needs `strict = false` explicitly — under
/// the integration job's ambient `LUNARIS_CONFORMANCE_STRICT=1` it would
/// otherwise panic, which is correct behaviour for a suite and wrong for the
/// test that checks that behaviour.
pub fn probe_url_env_with(env_name: &str, strict: bool) -> Option<String> {
    let url = match std::env::var(env_name) {
        Ok(u) if !u.is_empty() => u,
        _ => {
            crate::strict_skip::note_unavailable_with(format!("{env_name} unset"), strict);
            return None;
        }
    };
    probe_url_with(env_name, &url, strict).then_some(url)
}

/// Probe an already-obtained URL. Split out so a caller holding the URL from
/// somewhere other than the environment reports failures identically.
pub fn probe_url_with(what: &str, url: &str, strict: bool) -> bool {
    let Some(host_port) = host_port_of(url) else {
        crate::strict_skip::note_unavailable_with(
            format!("{what} (unknown URL scheme in {url})"),
            strict,
        );
        return false;
    };
    let addrs: Vec<SocketAddr> = match host_port.to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(e) => {
            crate::strict_skip::note_unavailable_with(
                format!("{what} (DNS resolution of {host_port} failed: {e})"),
                strict,
            );
            return false;
        }
    };
    if addrs.is_empty() {
        crate::strict_skip::note_unavailable_with(
            format!("{what} ({host_port} resolved to no addresses)"),
            strict,
        );
        return false;
    }
    if any_reachable(&addrs, CONNECT_TIMEOUT) {
        return true;
    }
    // Name the addresses actually tried. The CI failure this module exists to
    // fix reported only "TCP probe to localhost:6390 failed", which is true of
    // both "nothing is listening" and "we tried the wrong one of two".
    let tried = addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
    crate::strict_skip::note_unavailable_with(
        format!("{what} (TCP probe to {host_port} failed; tried {tried})"),
        strict,
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A live address the OS picked, plus an address nothing is listening on.
    fn live_and_dead() -> (SocketAddr, SocketAddr) {
        let live = TcpListener::bind("127.0.0.1:0").expect("bind live");
        let live_addr = live.local_addr().expect("live addr");
        // Bind and immediately drop to obtain a port that is free — and so
        // refuses — without racing a fixed guess.
        let dead_addr = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind dead");
            l.local_addr().expect("dead addr")
        };
        std::mem::forget(live);
        (live_addr, dead_addr)
    }

    /// THE regression. `localhost` resolving to an unreachable `::1` before a
    /// reachable `127.0.0.1` is what made four live suites skip inside a job
    /// that had built, port-checked and guaranteed the Moon they skipped for.
    #[test]
    fn a_dead_first_address_does_not_hide_a_live_second_one() {
        let (live, dead) = live_and_dead();
        assert!(
            any_reachable(&[dead, live], Duration::from_secs(1)),
            "probe gave up on the first address; a hostname with two addresses \
             only ever tried one of them"
        );
    }

    /// The vacuity floor: if `any_reachable` said true for everything, the
    /// test above would pass while proving nothing.
    #[test]
    fn nothing_listening_is_still_unreachable() {
        let (_live, dead) = live_and_dead();
        assert!(!any_reachable(&[dead], Duration::from_secs(1)));
    }

    #[test]
    fn an_empty_address_list_is_unreachable() {
        assert!(!any_reachable(&[], Duration::from_secs(1)));
    }

    #[test]
    fn host_port_parsing_covers_the_schemes_the_suites_use() {
        assert_eq!(host_port_of("moon://localhost:6390").as_deref(), Some("localhost:6390"));
        assert_eq!(host_port_of("redis://127.0.0.1:6379/0").as_deref(), Some("127.0.0.1:6379"));
        assert_eq!(host_port_of("postgres://u:p@db/lunaris").as_deref(), Some("db:5432"));
        assert_eq!(host_port_of("postgresql://u:p@db:5555/x").as_deref(), Some("db:5555"));
        // A scheme we do not speak is a skip, not a guessed port.
        assert_eq!(host_port_of("mysql://db:3306"), None);
        assert_eq!(host_port_of("no-scheme-at-all"), None);
    }

    /// An unset variable must be reportable WITHOUT the process environment
    /// deciding whether that is fatal — see the note on `probe_url_env_with`.
    #[test]
    fn an_unset_variable_is_none_on_a_dev_box() {
        assert!(probe_url_env_with("LUNARIS_LIVE_PROBE_DEFINITELY_UNSET", false).is_none());
    }

    #[test]
    fn an_unset_variable_is_fatal_under_strict() {
        assert!(
            std::panic::catch_unwind(|| {
                probe_url_env_with("LUNARIS_LIVE_PROBE_DEFINITELY_UNSET", true)
            })
            .is_err(),
            "strict mode returned None instead of refusing to skip"
        );
    }
}
