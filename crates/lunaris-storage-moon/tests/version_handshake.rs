//! Connect-time **Moon server version handshake** (0.6.2 task 7).
//!
//! Now that the supported deployment is an EXTERNAL Moon (install.sh / ghcr
//! image) rather than the vendored submodule, a Lunaris handle can be pointed at
//! any Moon build — including one that predates the `FT.*` / graph-Cypher /
//! `TEMPORAL.*` surface Lunaris depends on. Without a handshake the mismatch
//! surfaces much later as an opaque `ERR unknown command` on the first recall.
//!
//! ## The version field (vendor evidence)
//!
//! Moon's `INFO` `# Server` section emits THREE fields —
//! `vendor/moon/src/command/connection.rs:176-180`:
//!
//! ```text
//! # Server
//! redis_version:7.4.0        <- HARD-CODED compat literal (connection.rs:20)
//! moon_version:0.8.5         <- the REAL version, env!("CARGO_PKG_VERSION") (connection.rs:12)
//! moon:true
//! ```
//!
//! So `redis_version` is useless for gating (it never changes) and
//! `moon_version` is the only truthful signal. Note also that Moon's `INFO`
//! IGNORES its section argument (`pub fn info(db: &Database, _args: &[Frame])`,
//! `vendor/moon/src/command/connection.rs:172`) — `INFO server` returns the full
//! dump. The handshake must therefore scan, not assume a section-scoped reply.
//!
//! ## Policy under test
//!
//! | INFO reply                         | connect        |
//! |------------------------------------|----------------|
//! | `moon_version` < `MIN_MOON_VERSION`| HARD ERROR     |
//! | `moon_version` >= min              | ok (debug log) |
//! | no `moon_version` (plain Redis)    | ok (WARN once) |
//! | `INFO` rejected by the server      | ok (WARN once) |
//!
//! The last two rows are what keeps the dev/fake-server flows alive: a scripted
//! RESP fake that never implemented `INFO` must NOT start failing to connect.
//!
//! No live Moon and no model weights — CI-friendly.

use lunaris_core::error::StorageError;
use lunaris_storage_moon::MoonClient;
use std::io::{Read, Write};
use std::time::Duration;

/// How the scripted fake answers `INFO`.
#[derive(Clone)]
enum InfoScript {
    /// Reply with this exact bulk-string payload.
    Payload(String),
    /// Reply `-ERR unknown command` — models a fake / cut-down server that
    /// never implemented `INFO` at all.
    Unsupported,
}

/// Spawn a fake Moon that speaks enough RESP to carry `connect_with_dim` all the
/// way through: it completes the redis-rs multiplexed handshake, answers `INFO`
/// per `script`, reports an empty index list for `FT._LIST`, and `+OK`s
/// everything else (`FT.CREATE`, `GRAPH.CREATE`, …). Returns the bound port.
///
/// The listener, threads, and sockets are deliberately leaked for the lifetime
/// of the (short) test process — same convention as `op_timeout.rs`.
fn spawn_scripted_moon(script: InfoScript) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake-moon listener");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            let script = script.clone();
            std::thread::spawn(move || {
                sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
                let mut pending: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => pending.extend_from_slice(&buf[..n]),
                        Err(_) => return,
                    }
                    // Drain every COMPLETE command currently buffered. redis-rs
                    // pipelines its two `CLIENT SETINFO` setup commands, so a
                    // single read can carry more than one.
                    loop {
                        match parse_command(&pending) {
                            Ok(Some((args, used))) => {
                                pending.drain(..used);
                                let reply = reply_for(&args, &script);
                                if sock.write_all(&reply).is_err() {
                                    return;
                                }
                            }
                            // Incomplete — wait for more bytes.
                            Ok(None) => break,
                            // Malformed. Close rather than spin, so a parser bug
                            // fails the test fast instead of hanging it.
                            Err(()) => return,
                        }
                    }
                }
            });
        }
    });
    port
}

/// Canned reply for one parsed command.
fn reply_for(args: &[Vec<u8>], script: &InfoScript) -> Vec<u8> {
    let name =
        String::from_utf8_lossy(args.first().map(Vec::as_slice).unwrap_or(b"")).to_uppercase();
    match name.as_str() {
        "INFO" => match script {
            InfoScript::Payload(body) => {
                let mut out = format!("${}\r\n", body.len()).into_bytes();
                out.extend_from_slice(body.as_bytes());
                out.extend_from_slice(b"\r\n");
                out
            }
            InfoScript::Unsupported => b"-ERR unknown command 'INFO'\r\n".to_vec(),
        },
        // No pre-existing indices, so `assert_existing_index_dims_match` has
        // nothing to probe and `ensure_indexes` creates all four.
        "FT._LIST" => b"*0\r\n".to_vec(),
        _ => b"+OK\r\n".to_vec(),
    }
}

/// Parse ONE complete RESP command array (`*N\r\n` + N bulk strings) from the
/// front of `buf`. `Ok(Some((args, consumed)))` on success, `Ok(None)` when more
/// bytes are needed, `Err(())` when the bytes are not a RESP array at all.
#[allow(clippy::type_complexity)]
fn parse_command(buf: &[u8]) -> Result<Option<(Vec<Vec<u8>>, usize)>, ()> {
    let Some(&first) = buf.first() else { return Ok(None) };
    if first != b'*' {
        return Err(());
    }
    let mut i = 1;
    let Some((count, used)) = read_line_int(&buf[i..]) else { return Ok(None) };
    i += used;
    let mut args = Vec::with_capacity(count.max(0) as usize);
    for _ in 0..count {
        match buf.get(i) {
            None => return Ok(None),
            Some(b'$') => {}
            Some(_) => return Err(()),
        }
        i += 1;
        let Some((len, used)) = read_line_int(&buf[i..]) else { return Ok(None) };
        i += used;
        let len = len.max(0) as usize;
        if buf.len() < i + len + 2 {
            return Ok(None);
        }
        args.push(buf[i..i + len].to_vec());
        i += len + 2;
    }
    Ok(Some((args, i)))
}

/// Read `<int>\r\n`; returns the value and the bytes consumed (CRLF included).
fn read_line_int(buf: &[u8]) -> Option<(i64, usize)> {
    let pos = buf.windows(2).position(|w| w == b"\r\n")?;
    let value = std::str::from_utf8(&buf[..pos]).ok()?.parse().ok()?;
    Some((value, pos + 2))
}

/// A realistic Moon `# Server` INFO section carrying `version` as `moon_version`.
fn moon_info(version: &str) -> String {
    format!(
        "# Server\r\nredis_version:7.4.0\r\nmoon_version:{version}\r\nmoon:true\r\n\r\n\
         # Clients\r\nconnected_clients:1\r\n\r\n"
    )
}

/// (a) A Moon older than the minimum supported version MUST fail the connect
/// outright, and the message must name BOTH versions so the operator knows what
/// they have and what they need.
#[tokio::test]
async fn stale_moon_version_fails_connect_naming_both_versions() {
    let port = spawn_scripted_moon(InfoScript::Payload(moon_info("0.8.0")));
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    let Err(StorageError::Backend(msg)) = result else {
        panic!("a Moon older than the minimum supported version must fail connect, got {result:?}");
    };
    assert!(
        msg.contains("0.8.0"),
        "the error must name the version actually FOUND (0.8.0); got: {msg}"
    );
    assert!(
        msg.contains("0.8.5"),
        "the error must name the minimum version REQUIRED (0.8.5); got: {msg}"
    );
    let lower = msg.to_ascii_lowercase();
    assert!(
        lower.contains("upgrade"),
        "the error must be actionable — tell the operator to upgrade; got: {msg}"
    );
}

/// (b) An `INFO` reply with no `moon_version` at all (a plain Redis, say) must
/// NOT brick the connect: warn and continue.
#[tokio::test]
async fn absent_moon_version_warns_but_connects() {
    let plain_redis =
        "# Server\r\nredis_version:7.2.4\r\nredis_mode:standalone\r\n\r\n".to_string();
    let port = spawn_scripted_moon(InfoScript::Payload(plain_redis));
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(
        result.is_ok(),
        "an INFO reply without `moon_version` must WARN and continue, not fail: {result:?}"
    );
}

/// (b') An `INFO` value that is not a parseable `X.Y.Z` must also warn, not fail
/// — a dev build could label itself anything.
#[tokio::test]
async fn unparseable_moon_version_warns_but_connects() {
    let port = spawn_scripted_moon(InfoScript::Payload(moon_info("nightly-deadbeef")));
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(
        result.is_ok(),
        "an unparseable `moon_version` must WARN and continue, not fail: {result:?}"
    );
}

/// (c) REGRESSION GUARD for every existing fake/dev harness: a server that
/// rejects `INFO` outright must still connect. The handshake fails OPEN on a
/// server-side rejection — the server answered, it just answered "no".
#[tokio::test]
async fn server_that_rejects_info_still_connects() {
    let port = spawn_scripted_moon(InfoScript::Unsupported);
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(
        result.is_ok(),
        "a server that does not implement INFO must WARN and continue — otherwise every \
         scripted RESP fake in the suite stops connecting: {result:?}"
    );
}

/// The minimum version itself is SUPPORTED (`>=`, not `>`), and a newer one is
/// too. Pins the boundary so a future bump cannot silently become exclusive.
#[tokio::test]
async fn minimum_and_newer_versions_connect() {
    for version in ["0.8.5", "0.9.0", "1.0.0", "0.8.5-dev"] {
        let port = spawn_scripted_moon(InfoScript::Payload(moon_info(version)));
        let url = format!("moon://127.0.0.1:{port}");
        let result = MoonClient::connect_with_dim(&url, 768).await;
        assert!(result.is_ok(), "moon_version {version} must be accepted, got {result:?}");
    }
}
