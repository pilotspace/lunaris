//! RED suite for 0.6.2 task 9 — Moon's `read_as_of` MUST NOT answer a
//! *historical* snapshot request with present-time data.
//!
//! ## The bug this pins
//!
//! Moon KV rows are plain hashes (`HSET <key> v <value>`). `HGET`/`HMGET`
//! accept no `AS_OF` clause and Moon keeps no KV version chain, so
//! `read_as_of(scope, key, t)` has always returned **current state** no
//! matter how far in the past `t` is. Every caller that pins a historical
//! snapshot therefore received today's data labelled as yesterday's — the
//! bi-temporal contract silently violated on the primary backend.
//!
//! The fix is not to invent MVCC on top of hashes: it is to say so. A
//! historical `read_as_of` returns the typed
//! `StorageError::NotSupported(_)`, which `lunaris-server`'s `map_error`
//! already renders as `501 { "error": "not_supported" }`.
//!
//! Latest-state reads (`as_of` at/after "now", which is what every
//! production call site passes — `clock.tick()` immediately before the
//! read) keep working unchanged: returning current state IS the correct
//! answer for a latest read.
//!
//! ## Why this test needs no live Moon
//!
//! The fake RESP server below speaks exactly enough of the protocol to get
//! `MoonStorage::connect` through the redis-rs handshake, `FT._LIST`,
//! `FT.CREATE`, and the `MQ` capability probe. `read_as_of` is then driven
//! through the real `StoragePort` trait object.
//!
//! * TODAY → the historical read reaches `HMGET`, the fake answers two
//!   nils, the adapter returns `Ok(None)` → RED.
//! * FIXED → the guard fires before any RESP traffic →
//!   `Err(StorageError::NotSupported(_))` → GREEN.
//!
//! The `hmget_calls` counter makes the "before any RESP traffic" half of
//! the contract observable: a rejected historical read must cost zero Moon
//! round trips.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::{Scope, StoragePort};
use lunaris_storage_moon::MoonStorage;

/// Number of `HMGET` commands ONE fake server has served. `read_as_of` is
/// the only `HMGET` caller in the adapter, so this counts exactly the reads
/// that reached the wire. One counter per spawned server keeps concurrently
/// running `#[tokio::test]`s out of each other's tally.
type HmgetCounter = Arc<AtomicUsize>;

// ---------------------------------------------------------------------------
// Fake Moon — a minimal RESP2 responder
// ---------------------------------------------------------------------------

/// Split a RESP request buffer into complete commands (`Vec<Vec<u8>>` of
/// arguments), returning the parsed commands and the number of bytes
/// consumed. Inline (non-array) commands are not produced by redis-rs, so
/// only `*N` arrays are handled.
fn parse_commands(buf: &[u8]) -> (Vec<Vec<Vec<u8>>>, usize) {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some((argc, mut p)) = read_prefixed_len(buf, pos, b'*') {
        let mut args: Vec<Vec<u8>> = Vec::with_capacity(argc);
        let mut complete = true;
        for _ in 0..argc {
            let Some((len, after_hdr)) = read_prefixed_len(buf, p, b'$') else {
                complete = false;
                break;
            };
            if after_hdr + len + 2 > buf.len() {
                complete = false;
                break;
            }
            args.push(buf[after_hdr..after_hdr + len].to_vec());
            p = after_hdr + len + 2;
        }
        if !complete {
            break;
        }
        out.push(args);
        pos = p;
    }
    (out, pos)
}

/// Read a `<marker><len>\r\n` header at `pos`; returns `(len, index just
/// past the CRLF)`.
fn read_prefixed_len(buf: &[u8], pos: usize, marker: u8) -> Option<(usize, usize)> {
    if pos >= buf.len() || buf[pos] != marker {
        return None;
    }
    let crlf = buf[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
    let n: usize = std::str::from_utf8(&buf[pos + 1..crlf]).ok()?.trim().parse().ok()?;
    Some((n, crlf + 2))
}

/// Canned reply for one command. Unknown commands get `+OK` — the point of
/// the fake is the `read_as_of` path, not Moon fidelity.
fn reply_for(args: &[Vec<u8>], hmgets: &HmgetCounter) -> Vec<u8> {
    let name =
        String::from_utf8_lossy(args.first().map(|a| a.as_slice()).unwrap_or(b"")).to_uppercase();
    if std::env::var_os("FAKE_MOON_TRACE").is_some() {
        let rendered: Vec<String> =
            args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        eprintln!("fake-moon <= {rendered:?}");
    }
    match name.as_str() {
        // Index inventory probe (`assert_existing_index_dims_match`): no
        // pre-existing indices, so the dim guardrail is a no-op.
        "FT._LIST" => b"*0\r\n".to_vec(),
        // `MQ DLQLEN` capability probe wants an integer.
        "MQ" => b":0\r\n".to_vec(),
        // The read under test: both requested fields (`v`, `bt`) are nil,
        // i.e. "key absent" — today's adapter turns that into `Ok(None)`.
        "HMGET" => {
            hmgets.fetch_add(1, Ordering::SeqCst);
            b"*2\r\n$-1\r\n$-1\r\n".to_vec()
        }
        _ => b"+OK\r\n".to_vec(),
    }
}

/// Spawn the fake Moon on an ephemeral port; returns the port. The
/// listener and its per-connection threads are deliberately leaked for the
/// (short) lifetime of the test process.
fn spawn_fake_moon() -> (u16, HmgetCounter) {
    let hmgets: HmgetCounter = Arc::new(AtomicUsize::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake-moon listener");
    let port = listener.local_addr().expect("local_addr").port();
    let server_hmgets = Arc::clone(&hmgets);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            let hmgets = Arc::clone(&server_hmgets);
            std::thread::spawn(move || {
                sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
                let mut pending: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            pending.extend_from_slice(&buf[..n]);
                            let (cmds, consumed) = parse_commands(&pending);
                            pending.drain(..consumed);
                            let mut out = Vec::new();
                            for c in &cmds {
                                out.extend_from_slice(&reply_for(c, &hmgets));
                            }
                            if !out.is_empty() && sock.write_all(&out).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    (port, hmgets)
}

async fn fake_moon_storage() -> (Arc<dyn StoragePort>, HmgetCounter) {
    let (port, hmgets) = spawn_fake_moon();
    let url = format!("moon://127.0.0.1:{port}");
    let storage: Arc<dyn StoragePort> =
        Arc::new(MoonStorage::connect(&url).await.expect("connect to fake moon"));
    (storage, hmgets)
}

/// A timestamp unambiguously in the past — 1970-01-01T00:00:01Z. No live
/// read could ever legitimately pin this.
fn historical() -> Hlc {
    Hlc::from_parts(1_000, 0, 0)
}

fn now_hlc() -> Hlc {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Hlc::from_parts(ms, 0, 0)
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// PRIMARY RED: a historical pin must surface the typed `NotSupported`
/// error, never present-time data (and never a silent `Ok(None)`, which
/// reads to the caller as "nothing existed at that instant" — a claim Moon
/// cannot make).
#[tokio::test]
async fn historical_read_as_of_is_explicitly_unsupported() {
    let (storage, hmgets) = fake_moon_storage().await;
    let scope = Scope::new("t-1").expect("valid scope");

    let got = storage.read_as_of(&scope, b"lunaris:t-1:episode:X", historical()).await;

    assert!(
        matches!(got, Err(StorageError::NotSupported(_))),
        "Moon KV has no AS_OF clause: a historical read_as_of MUST return \
         StorageError::NotSupported (→ HTTP 501 not_supported via lunaris-server's map_error), \
         never present-time data and never a bare Ok(None). Got: {got:?}"
    );
    assert_eq!(
        hmgets.load(Ordering::SeqCst),
        0,
        "an unsupported historical read must be rejected BEFORE any Moon round trip \
         (the guard is a pure function of `as_of`, not a post-hoc filter on the reply)"
    );
}

/// The error message must name the operation so an operator reading a 501
/// body knows which capability is missing and where the supported path is.
#[tokio::test]
async fn unsupported_error_message_is_actionable() {
    let (storage, _hmgets) = fake_moon_storage().await;
    let scope = Scope::new("t-1").expect("valid scope");

    let Err(err) = storage.read_as_of(&scope, b"lunaris:t-1:episode:X", historical()).await else {
        panic!("historical read must be an error");
    };
    let msg = err.to_string();
    assert!(msg.contains("read_as_of"), "message must name the operation, got: {msg}");
    assert!(
        msg.to_lowercase().contains("moon"),
        "message must name the backend that lacks the capability, got: {msg}"
    );
}

/// GUARD AGAINST OVER-CORRECTION: every production call site reads "latest"
/// by ticking the clock immediately before the call. Those reads MUST keep
/// working — returning current state is the *correct* answer for a
/// latest-state read, and erroring here would take down recall, hydrate,
/// forget, verify and the whole HTTP surface on Moon.
#[tokio::test]
async fn latest_read_as_of_still_serves_current_state() {
    let (storage, hmgets) = fake_moon_storage().await;
    let scope = Scope::new("t-1").expect("valid scope");

    let got = storage.read_as_of(&scope, b"lunaris:t-1:episode:X", now_hlc()).await;

    assert!(
        matches!(got, Ok(None)),
        "a latest-state read must still hit Moon and report the row's absence, got {got:?}"
    );
    assert_eq!(
        hmgets.load(Ordering::SeqCst),
        1,
        "a latest-state read must reach Moon (exactly one HMGET round trip)"
    );
}

/// A future `as_of` is also a latest-state read (`u64::MAX / 2` is what the
/// conformance suite pins post-commit). It must not be mistaken for a
/// historical pin.
#[tokio::test]
async fn future_as_of_is_treated_as_latest() {
    let (storage, _hmgets) = fake_moon_storage().await;
    let scope = Scope::new("t-1").expect("valid scope");

    let far_future = Hlc::from_parts(u64::MAX / 2, 0, 0);
    let got = storage.read_as_of(&scope, b"lunaris:t-1:episode:X", far_future).await;
    assert!(matches!(got, Ok(None)), "a future as_of is a latest read, got {got:?}");
}

/// The backend must ALSO advertise the gap, so callers (and the
/// conformance suite) can route as-of reads to a backend that answers them
/// instead of discovering the hole at query time.
#[tokio::test]
async fn moon_advertises_no_historical_kv_reads() {
    let (storage, _hmgets) = fake_moon_storage().await;
    assert!(
        !storage.supports_historical_kv_reads(),
        "MoonStorage must declare `supports_historical_kv_reads() == false` — the default \
         StoragePort answer is `true`, so a silent default here is the overclaim itself"
    );
}
