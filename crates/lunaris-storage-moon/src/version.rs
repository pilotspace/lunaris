//! Connect-time **Moon server version handshake**.
//!
//! ## Why this exists
//!
//! The supported deployment is an EXTERNAL Moon instance (`install.sh`, the
//! ghcr image, or an operator-managed binary), not the `vendor/moon` submodule
//! this workspace builds its SDK against. A Lunaris handle can therefore be
//! pointed at *any* Moon build — including one that predates the `FT.*` search,
//! graph-Cypher, or `TEMPORAL.*` surface Lunaris depends on. Without a
//! handshake that mismatch stays invisible until the first recall, where it
//! surfaces as an opaque `ERR unknown command 'FT.SEARCH'` from deep inside a
//! retrieval leg. One `INFO` round-trip at connect converts that into an
//! actionable startup failure.
//!
//! ## The wire format (vendor evidence, `vendor/moon` @ v0.8.5)
//!
//! `vendor/moon/src/command/connection.rs:176-180` writes the `# Server`
//! section, verbatim:
//!
//! ```text
//! sections.push_str("# Server\r\n");
//! let _ = write!(sections, "redis_version:{REDIS_COMPAT_VERSION}\r\n");
//! let _ = write!(sections, "moon_version:{MOON_VERSION}\r\n");
//! sections.push_str("moon:true\r\n");
//! ```
//!
//! producing
//!
//! ```text
//! # Server
//! redis_version:7.4.0
//! moon_version:0.8.5
//! moon:true
//! ```
//!
//! Two facts drive this module's design:
//!
//! 1. **`redis_version` is a hard-coded compatibility lie.**
//!    `vendor/moon/src/command/connection.rs:20` declares
//!    `pub const REDIS_COMPAT_VERSION: &str = "7.4.0";` — a literal that never
//!    tracks a Moon release. `vendor/moon/src/command/connection.rs:12` declares
//!    `pub const MOON_VERSION: &str = env!("CARGO_PKG_VERSION");` — the real
//!    one. We gate on `moon_version` and IGNORE `redis_version` entirely.
//! 2. **`INFO` ignores its section argument.**
//!    `vendor/moon/src/command/connection.rs:172` is
//!    `pub fn info(db: &Database, _args: &[Frame]) -> Frame` — `_args` is never
//!    read, so `INFO server` returns the *whole* dump (and the handler layer can
//!    even append a second `# Replication` header,
//!    `vendor/moon/src/server/conn/handler_monoio/dispatch.rs:678-684`). So this
//!    parser SCANS for the field across all lines rather than assuming a
//!    section-scoped, single-occurrence reply.
//!
//! `HELLO` also carries the real version
//! (`vendor/moon/src/command/connection.rs:679-682`, key `version`), but its
//! reply shape differs between RESP2 and RESP3 and redis-rs owns the `HELLO`
//! exchange during connection setup — `INFO` is the seam we can drive ourselves
//! without fighting the driver.
//!
//! ## Policy
//!
//! | `INFO` reply                          | outcome                       |
//! |---------------------------------------|-------------------------------|
//! | `moon_version` < [`MIN_MOON_VERSION`] | HARD ERROR at connect         |
//! | `moon_version` >= min                 | `debug!`, connect proceeds    |
//! | no / unparseable `moon_version`       | `warn!` once, connect proceeds|
//! | server REJECTED `INFO`                | `warn!` once, connect proceeds|
//! | server never ANSWERED `INFO`          | HARD ERROR (see below)        |
//!
//! Failing **open** on an unrecognizable reply is deliberate: a plain Redis, a
//! cut-down dev build, or one of the scripted RESP fakes in the test suite would
//! otherwise become unusable, and this handshake's job is to explain a
//! mismatch, not to police what a developer may dial. Failing **closed** on a
//! timeout / dropped socket is equally deliberate — the connection itself is
//! broken, and warning past that would just defer the same failure to
//! `FT._LIST` one command later (see [`info_probe_failure_is_fatal`]).

use std::fmt;

/// Minimum Moon server version Lunaris supports.
///
/// Tied to the `vendor/moon` submodule pin — currently tag **v0.8.7**
/// (`c9e818f973b3b6d6ede3ee04d8ad63b40014959d`), the version this workspace's
/// `moondb` SDK is built against. When the pin moves, move this with it only if
/// the new pin brings a *required* server-side capability; a pin bump alone is
/// not a reason to lock operators out of an older, still-sufficient server.
pub const MIN_MOON_VERSION: MoonVersion = MoonVersion { major: 0, minor: 8, patch: 5 };

/// The `INFO` field carrying Moon's real version
/// (`vendor/moon/src/command/connection.rs:178`). Deliberately NOT
/// `redis_version`, which is the hard-coded `"7.4.0"` compat literal at
/// `vendor/moon/src/command/connection.rs:20`.
pub const MOON_VERSION_FIELD: &str = "moon_version";

/// A `major.minor.patch` server version.
///
/// Any pre-release / build suffix (`-dev`, `-rc.1`, `+meta`) is parsed off and
/// DISCARDED: `0.8.5-dev` compares equal to `0.8.5`. Operators routinely run
/// off-tag builds whose `CARGO_PKG_VERSION` has not been bumped yet, and
/// refusing those would be a worse failure than the one this guard prevents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MoonVersion {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
}

impl fmt::Display for MoonVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl MoonVersion {
    /// Parse `X.Y.Z`, tolerating a `-prerelease` / `+build` suffix and
    /// surrounding whitespace. Returns `None` for anything else — including
    /// `X.Y` and bare `X`, which are too ambiguous to gate on.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let core = raw.trim();
        let core = core.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { major, minor, patch })
    }
}

/// Verdict of [`check_moon_version`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoonVersionCheck {
    /// `moon_version` parsed and is `>= MIN_MOON_VERSION`.
    Supported(MoonVersion),
    /// `moon_version` parsed and is strictly older than [`MIN_MOON_VERSION`].
    Unsupported(MoonVersion),
    /// No usable `moon_version` in the reply. Carries a human-readable account
    /// of what WAS seen, so the warning names the actual observation instead of
    /// a generic "could not determine".
    Unknown(String),
}

/// Classify an `INFO` reply body against [`MIN_MOON_VERSION`].
///
/// Pure and total — every input yields a verdict, so the async seam in
/// `client.rs` holds no parsing logic and this can be unit-tested without a
/// server.
#[must_use]
pub fn check_moon_version(info: &str) -> MoonVersionCheck {
    match info_field(info, MOON_VERSION_FIELD) {
        Some(raw) => match MoonVersion::parse(raw) {
            Some(v) if v >= MIN_MOON_VERSION => MoonVersionCheck::Supported(v),
            Some(v) => MoonVersionCheck::Unsupported(v),
            None => MoonVersionCheck::Unknown(format!(
                "the server reported `{MOON_VERSION_FIELD}:{raw}`, which is not a parseable \
                 major.minor.patch version"
            )),
        },
        None => match info_field(info, "redis_version") {
            Some(redis) => MoonVersionCheck::Unknown(format!(
                "the INFO reply carried no `{MOON_VERSION_FIELD}` field — only \
                 `redis_version:{redis}`, which Moon hard-codes to a compatibility literal and \
                 every plain Redis also reports. This is most likely NOT a Moon server"
            )),
            None => MoonVersionCheck::Unknown(format!(
                "the INFO reply carried neither `{MOON_VERSION_FIELD}` nor `redis_version` \
                 (first bytes seen: {:?})",
                info.chars().take(120).collect::<String>()
            )),
        },
    }
}

/// Extract `name:<value>` from a Redis `INFO` body.
///
/// Matches only at the START of a line so a field name occurring inside another
/// field's value cannot be mistaken for the field itself. Comment lines (`#`)
/// and blank lines are skipped. The FIRST occurrence wins — Moon's handler layer
/// can append a duplicate section
/// (`vendor/moon/src/server/conn/handler_monoio/dispatch.rs:678-684`), and the
/// canonical `# Server` block is always emitted first
/// (`vendor/moon/src/command/connection.rs:176`).
pub(crate) fn info_field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).find_map(
        |line| {
            let (key, value) = line.split_once(':')?;
            if key == name { Some(value.trim()) } else { None }
        },
    )
}

/// Should a failed `INFO` probe abort the connect?
///
/// `true` for transport-level faults (timeout, dropped socket, IO error): the
/// connection is unusable, and continuing would only re-fail one command later
/// with a less informative message. Crucially this is what keeps
/// `tests/op_timeout.rs` discriminating — the handshake is now the first real
/// command on the wire, so a stalled server must still surface as a bounded
/// `StorageError::Backend`.
///
/// `false` for anything the SERVER answered with (`-ERR unknown command`,
/// `NOPERM`, a wrong-type reply): the peer is alive and talking, it just does
/// not offer `INFO`. That is the fail-open case — see the module docs.
#[must_use]
pub fn info_probe_failure_is_fatal(err: &redis::RedisError) -> bool {
    err.is_timeout() || err.is_io_error() || err.is_connection_dropped()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact `# Server` block Moon emits
    /// (`vendor/moon/src/command/connection.rs:176-180`), plus a following
    /// section so the parser is exercised against a multi-section dump.
    fn moon_info(version: &str) -> String {
        format!(
            "# Server\r\nredis_version:7.4.0\r\nmoon_version:{version}\r\nmoon:true\r\n\r\n\
             # Clients\r\nconnected_clients:1\r\n"
        )
    }

    #[test]
    fn parses_the_real_moon_version_field_not_the_redis_compat_literal() {
        // A Moon reporting 7.4.0 as `redis_version` must NOT be read as 7.4.0.
        assert_eq!(
            check_moon_version(&moon_info("0.8.5")),
            MoonVersionCheck::Supported(MoonVersion { major: 0, minor: 8, patch: 5 })
        );
    }

    #[test]
    fn the_minimum_version_itself_is_supported() {
        assert_eq!(
            check_moon_version(&moon_info(&MIN_MOON_VERSION.to_string())),
            MoonVersionCheck::Supported(MIN_MOON_VERSION),
            "the bound is inclusive (>=), not exclusive"
        );
    }

    #[test]
    fn older_patch_minor_and_major_are_all_unsupported() {
        for (raw, expect) in [
            ("0.8.4", MoonVersion { major: 0, minor: 8, patch: 4 }),
            ("0.7.9", MoonVersion { major: 0, minor: 7, patch: 9 }),
            ("0.0.1", MoonVersion { major: 0, minor: 0, patch: 1 }),
        ] {
            assert_eq!(
                check_moon_version(&moon_info(raw)),
                MoonVersionCheck::Unsupported(expect),
                "{raw} is older than {MIN_MOON_VERSION}"
            );
        }
    }

    #[test]
    fn newer_versions_are_supported() {
        for raw in ["0.8.6", "0.9.0", "1.0.0", "10.0.0"] {
            assert!(
                matches!(check_moon_version(&moon_info(raw)), MoonVersionCheck::Supported(_)),
                "{raw} is newer than {MIN_MOON_VERSION}"
            );
        }
    }

    #[test]
    fn prerelease_and_build_suffixes_compare_on_the_numeric_core() {
        assert!(matches!(
            check_moon_version(&moon_info("0.8.5-dev")),
            MoonVersionCheck::Supported(_)
        ));
        assert!(matches!(
            check_moon_version(&moon_info("0.9.0-rc.1")),
            MoonVersionCheck::Supported(_)
        ));
        assert!(matches!(
            check_moon_version(&moon_info("0.8.4+abcdef")),
            MoonVersionCheck::Unsupported(_)
        ));
    }

    #[test]
    fn a_plain_redis_is_unknown_and_the_detail_says_so() {
        let plain = "# Server\r\nredis_version:7.2.4\r\nredis_mode:standalone\r\n";
        let MoonVersionCheck::Unknown(detail) = check_moon_version(plain) else {
            panic!("a plain Redis has no moon_version, so the verdict must be Unknown");
        };
        assert!(detail.contains("7.2.4"), "the detail must report what was seen: {detail}");
        assert!(
            detail.contains("moon_version"),
            "the detail must name the missing field: {detail}"
        );
    }

    #[test]
    fn an_unparseable_version_is_unknown_and_echoes_the_raw_value() {
        let MoonVersionCheck::Unknown(detail) = check_moon_version(&moon_info("nightly-deadbeef"))
        else {
            panic!("an unparseable version must be Unknown, never silently Supported");
        };
        assert!(detail.contains("nightly"), "the detail must echo the raw value: {detail}");
    }

    #[test]
    fn an_empty_or_junk_reply_is_unknown_not_a_panic() {
        assert!(matches!(check_moon_version(""), MoonVersionCheck::Unknown(_)));
        assert!(matches!(check_moon_version("total garbage"), MoonVersionCheck::Unknown(_)));
    }

    #[test]
    fn a_field_name_embedded_in_another_value_is_not_mistaken_for_the_field() {
        // `os:` carries the literal text `moon_version:9.9.9`. Matching mid-line
        // would read 9.9.9 and wave through an ancient server.
        let spoofed = "# Server\r\nos:built with moon_version:9.9.9\r\nmoon_version:0.1.0\r\n";
        assert_eq!(
            check_moon_version(spoofed),
            MoonVersionCheck::Unsupported(MoonVersion { major: 0, minor: 1, patch: 0 }),
            "only a line-leading `moon_version:` counts"
        );
    }

    #[test]
    fn parse_rejects_partial_and_over_long_versions() {
        assert!(MoonVersion::parse("1").is_none());
        assert!(MoonVersion::parse("1.2").is_none());
        assert!(MoonVersion::parse("1.2.3.4").is_none());
        assert!(MoonVersion::parse("").is_none());
        assert_eq!(
            MoonVersion::parse(" 1.2.3 "),
            Some(MoonVersion { major: 1, minor: 2, patch: 3 }),
            "surrounding whitespace from the INFO line must not defeat the parse"
        );
    }

    #[test]
    fn ordering_is_major_then_minor_then_patch() {
        let v = |a, b, c| MoonVersion { major: a, minor: b, patch: c };
        assert!(v(1, 0, 0) > v(0, 99, 99));
        assert!(v(0, 9, 0) > v(0, 8, 99));
        assert!(v(0, 8, 6) > v(0, 8, 5));
    }
}
