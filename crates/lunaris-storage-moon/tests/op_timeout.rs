//! RED suite for `io-failsafe-wiring` (Half B): a Moon command that gets no
//! response MUST be bounded by `LUNARIS_MOON_OP_TIMEOUT`, never an unbounded
//! hang and never silently capped at redis-rs's short built-in default.
//!
//! No live Moon and no model weights — CI-friendly. The fake server below speaks
//! exactly enough RESP to complete the redis-rs multiplexed-connection handshake
//! (two pipelined `CLIENT SETINFO` commands → `+OK` each), then STALLS on the
//! first real command. That puts the stall on an *established* connection,
//! which is where the per-op response timeout actually governs.
//!
//! Since 0.6.2 that first real command is `INFO server`, issued by
//! `connect_with_dim` via `MoonClient::version_handshake` (it used to be
//! `FT._LIST` via `assert_existing_index_dims_match`, one command later). The
//! contract is unchanged because the handshake classifies a TIMEOUT as fatal
//! and propagates it — see
//! `lunaris_storage_moon::version::info_probe_failure_is_fatal`. Only a
//! server-side *rejection* of `INFO` is warned past. If that classification
//! ever flipped to fail-open, the stall would fall through to `FT._LIST` and
//! cost a SECOND full timeout, tripping the `< 6s` upper bound below — so this
//! test also guards the fail-closed half of the handshake policy.
//!
//! ## Why this test discriminates (empirically measured 2026-06-15)
//!
//! * A connect-time black hole that never speaks is useless here: redis-rs caps
//!   connection *setup* at ~1s regardless of any per-op timeout, so it cannot
//!   tell the fix from the bug.
//! * On an *established* connection a stalled command is bounded by:
//!     - the redis-rs default (~500ms) when the connection was opened with the
//!       plain `connect` — this is today's code, which IGNORES
//!       `LUNARIS_MOON_OP_TIMEOUT`; and
//!     - the configured value when opened with `connect_with_timeout(N)` — the
//!       fix, which threads `moon_op_timeout()` (default 10s, env-overridable).
//!
//! So with `LUNARIS_MOON_OP_TIMEOUT=3`:
//!   * TODAY (`connect`)            → `connect_with_dim` errors in ~0.5s  → below the 2s floor → RED.
//!   * FIXED (`connect_with_timeout`) → `connect_with_dim` errors in ~3.0s → within [2s, 6s] → GREEN.
//!
//! (RED satisfiability per memory `feedback_red_satisfiability`.)

use lunaris_core::error::StorageError;
use lunaris_storage_moon::MoonClient;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Spawn a fake RESP server that completes the redis-rs multiplexed handshake
/// (acks each `CLIENT SETINFO` with `+OK`) and then stalls — it never answers
/// the first real command, holding the socket open so the client must rely on
/// its own per-op response timeout to give up. Returns the bound port. The
/// listener, connection threads, and sockets are deliberately leaked for the
/// lifetime of the (short) test process.
fn spawn_handshake_then_stall() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake-moon listener");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            std::thread::spawn(move || {
                sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
                let mut acked = 0usize;
                let mut seen: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => {
                            seen.extend_from_slice(&buf[..n]);
                            // Count complete `CLIENT SETINFO` setup commands seen so far.
                            let setinfo = seen.windows(7).filter(|w| *w == b"SETINFO").count();
                            // A chunk that adds no new SETINFO is the first real
                            // command (FT._LIST) — stop reading and stall on it.
                            if setinfo == acked && acked > 0 {
                                break;
                            }
                            while acked < setinfo {
                                if sock.write_all(b"+OK\r\n").is_err() {
                                    return;
                                }
                                acked += 1;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Park: hold the socket open, never answer the real command, so
                // the client's per-op response timeout is the only thing that
                // can end the wait.
                std::thread::sleep(Duration::from_secs(3600));
                drop(sock);
            });
        }
    });
    port
}

#[tokio::test]
async fn stalled_moon_command_bounded_by_configured_op_timeout() {
    let port = spawn_handshake_then_stall();
    let url = format!("moon://127.0.0.1:{port}");

    // SAFETY: single-threaded mutation of one process-unique env var. This is the
    // only test in this binary and the only reader of `LUNARIS_MOON_OP_TIMEOUT`;
    // it is removed before the assertions run.
    unsafe { std::env::set_var("LUNARIS_MOON_OP_TIMEOUT", "3") };

    let started = Instant::now();
    let result = MoonClient::connect_with_dim(&url, 768).await;
    let elapsed = started.elapsed();

    unsafe { std::env::remove_var("LUNARIS_MOON_OP_TIMEOUT") };

    assert!(
        matches!(result, Err(StorageError::Backend(_))),
        "a stalled Moon command must surface as a transient StorageError::Backend, got {result:?}"
    );
    // The configured 3s op-timeout must govern. Below 2s means the code is still
    // using redis-rs's ~500ms default (the bug); above 6s means it never bounded
    // the command at all (a different failure). Either way: not the contract.
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(6),
        "expected the configured LUNARIS_MOON_OP_TIMEOUT=3s to bound the stalled command \
         (elapsed {elapsed:?}); <2s = redis default still in force, >=6s = unbounded"
    );
}
