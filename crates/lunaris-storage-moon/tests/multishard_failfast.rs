//! Connect-time **single-shard guard** (0.7.0 task 22, RFC 0008 §6 Option C).
//!
//! ## Why this exists
//!
//! RFC 0008 (`docs/rfcs/0008-sharded-moon-ingest.md`) closed the question "can
//! Lunaris run on a sharded Moon?" with **no**, and for two independent reasons:
//!
//! * **Write side (§2.3).** Moon's `TXN.*` guard is not "all keys must
//!   co-locate", it is "all keys must land on **the connection's own shard**"
//!   (`vendor/moon/src/server/conn/handler_monoio/mod.rs:2600-2610`). Connections
//!   are assigned to shards round-robin (`listener.rs:503-508`) with no client
//!   control, so on a `--shards N` instance ingest fails on roughly `1 - 1/N` of
//!   connections — non-deterministically, per connection.
//! * **Read side (§1.3).** `FT.NAVIGATE` — which `navigate.rs:47` issues on the
//!   recall path — does **not** scatter-gather
//!   (`handler_monoio/ft.rs:554-560`, a bare `with_shard`). A Navigate for a
//!   scope living on another shard returns **empty, with no error**. Silent
//!   recall degradation is worse than a loud failure.
//!
//! Today that mismatch surfaces mid-ingest, inside a half-applied `TXN`. This
//! guard converts it into an actionable startup failure — the same trade the
//! version handshake (`tests/version_handshake.rs`) already makes.
//!
//! ## The probe under test
//!
//! Moon exposes **no shard count** (RFC §2.3: `INFO`, `CONFIG GET *shard*`,
//! `CLIENT INFO` and `CLUSTER KEYSLOT` all come up empty; upstream ask
//! pilotspace/moon#497). So the guard uses the RFC's co-location canary,
//! narrowed to its cheapest correct form: a **read-only** `MULTI` body whose
//! keys would hash to different shards.
//!
//! ```text
//! MULTI
//! EXISTS lunaris:__shardprobe__:canary:0 … :63     (one command, 64 keys)
//! EXEC
//! ```
//!
//! * `num_shards > 1` → `EXEC` is rejected `CROSSSLOT Keys in MULTI/EXEC don't
//!   hash to the same shard` (`handler_monoio/write.rs:849-853`, reached via
//!   `analyze_txn_locality`, `shared.rs:917-991`) — **and nothing is written**.
//! * `num_shards == 1` → the whole body executes and `EXEC` returns an array.
//!
//! Because the body is read-only there is no canary key to clean up on ANY path
//! — the strongest form of "leaves no residue". `probe_issues_no_write_command`
//! pins that.
//!
//! ## Policy under test
//!
//! | observation                                   | connect        |
//! |-----------------------------------------------|----------------|
//! | `INFO` carries `num_shards` > 1               | HARD ERROR     |
//! | `INFO` carries `num_shards` == 1              | ok (no probe)  |
//! | probe rejected `CROSSSLOT` / `cross-shard`    | HARD ERROR     |
//! | probe executed (array reply)                  | ok (debug log) |
//! | probe answered with any OTHER server error    | ok (WARN once) |
//! | probe reply shape not understood              | ok (WARN once) |
//! | probe hit a transport fault (timeout / IO)    | HARD ERROR     |
//!
//! The fail-open rows are what keep every scripted RESP fake in this suite
//! connectable — see `server_without_multi_support_still_connects`.
//!
//! No live Moon and no model weights — CI-friendly.

use lunaris_core::error::StorageError;
use lunaris_storage_moon::MoonClient;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How the scripted fake answers the co-location probe.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeScript {
    /// A MULTI-SHARD Moon: `MULTI` → `+OK`, body → `+QUEUED`, `EXEC` →
    /// `-CROSSSLOT …`. Verbatim the string Moon emits at
    /// `vendor/moon/src/server/conn/handler_monoio/write.rs:849-853`.
    CrossSlot,
    /// The same rejection phrased Moon's OTHER way — the `TXN.*` guard's
    /// wording (`ERR TXN does not support cross-shard writes …`, RFC 0008 §2.4).
    /// A server can only produce this if it is sharded, so it must be treated
    /// identically even though redis-rs classifies the code as a plain `ERR`.
    CrossShardWording,
    /// A SINGLE-SHARD Moon: the body executes and `EXEC` returns its array.
    SingleShard,
    /// A server with no `MULTI` at all — every probe command is rejected.
    NoMulti,
    /// The legacy fake shape used across this test suite: `+OK` to everything
    /// that is not explicitly scripted. The probe reply is therefore not an
    /// array and must be treated as inconclusive, NOT as a failure.
    AlwaysOk,
}

/// What the fake answers `INFO` with.
#[derive(Clone)]
enum InfoScript {
    /// Reply with this exact bulk-string payload.
    Payload(String),
    /// Reply `-ERR unknown command` — a cut-down server with no `INFO`.
    Unsupported,
}

#[derive(Clone)]
struct Script {
    info: InfoScript,
    probe: ProbeScript,
}

/// Every command name the fake saw, in order. Used by
/// `probe_issues_no_write_command` to prove the guard never writes.
type CommandLog = Arc<Mutex<Vec<String>>>;

/// Spawn a fake Moon that speaks enough RESP to carry `connect_with_dim` all the
/// way through, scripted per `script`. Returns the bound port and the command
/// log. Listener/threads/sockets are deliberately leaked for the lifetime of the
/// (short) test process — same convention as `version_handshake.rs`.
fn spawn_scripted_moon(script: Script) -> (u16, CommandLog) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake-moon listener");
    let port = listener.local_addr().expect("local_addr").port();
    let log: CommandLog = Arc::new(Mutex::new(Vec::new()));
    let log_for_thread = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            let script = script.clone();
            let log = Arc::clone(&log_for_thread);
            std::thread::spawn(move || {
                sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
                let mut pending: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => pending.extend_from_slice(&buf[..n]),
                        Err(_) => return,
                    }
                    // Drain every COMPLETE command currently buffered — the
                    // probe arrives as one pipelined write of three commands.
                    loop {
                        match parse_command(&pending) {
                            Ok(Some((args, used))) => {
                                pending.drain(..used);
                                log.lock().expect("command log poisoned").push(command_name(&args));
                                let reply = reply_for(&args, &script);
                                if sock.write_all(&reply).is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(()) => return,
                        }
                    }
                }
            });
        }
    });
    (port, log)
}

fn command_name(args: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(args.first().map(Vec::as_slice).unwrap_or(b"")).to_uppercase()
}

/// Canned reply for one parsed command.
fn reply_for(args: &[Vec<u8>], script: &Script) -> Vec<u8> {
    match command_name(args).as_str() {
        "INFO" => match &script.info {
            InfoScript::Payload(body) => {
                let mut out = format!("${}\r\n", body.len()).into_bytes();
                out.extend_from_slice(body.as_bytes());
                out.extend_from_slice(b"\r\n");
                out
            }
            InfoScript::Unsupported => b"-ERR unknown command 'INFO'\r\n".to_vec(),
        },
        "MULTI" => match script.probe {
            ProbeScript::NoMulti => b"-ERR unknown command 'MULTI'\r\n".to_vec(),
            _ => b"+OK\r\n".to_vec(),
        },
        "EXISTS" => match script.probe {
            ProbeScript::CrossSlot | ProbeScript::CrossShardWording | ProbeScript::SingleShard => {
                b"+QUEUED\r\n".to_vec()
            }
            ProbeScript::NoMulti => b"-ERR unknown command 'EXISTS'\r\n".to_vec(),
            ProbeScript::AlwaysOk => b"+OK\r\n".to_vec(),
        },
        "EXEC" => match script.probe {
            // Verbatim Moon's multi-shard rejection.
            ProbeScript::CrossSlot => {
                b"-CROSSSLOT Keys in MULTI/EXEC don't hash to the same shard\r\n".to_vec()
            }
            ProbeScript::CrossShardWording => {
                b"-ERR TXN does not support cross-shard writes -- use hash tags {tag} \
                  to co-locate keys\r\n"
                    .to_vec()
            }
            // One queued command → a one-element array. `:0` = none of the
            // probe keys exist, which is the normal answer.
            ProbeScript::SingleShard => b"*1\r\n:0\r\n".to_vec(),
            ProbeScript::NoMulti => b"-ERR EXEC without MULTI\r\n".to_vec(),
            ProbeScript::AlwaysOk => b"+OK\r\n".to_vec(),
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

/// A realistic Moon `# Server` INFO section at a supported version, so the
/// version handshake passes and the shard guard is what these tests exercise.
fn moon_info() -> InfoScript {
    InfoScript::Payload(
        "# Server\r\nredis_version:7.4.0\r\nmoon_version:0.8.5\r\nmoon:true\r\n\r\n\
         # Clients\r\nconnected_clients:1\r\n\r\n"
            .to_string(),
    )
}

/// Same, plus a `num_shards` field — the forward-compatible fast path for
/// upstream ask pilotspace/moon#497 (RFC 0008 §6, "expose `num_shards`").
fn moon_info_with_shards(n: u64) -> InfoScript {
    InfoScript::Payload(format!(
        "# Server\r\nredis_version:7.4.0\r\nmoon_version:0.8.5\r\nmoon:true\r\n\
         num_shards:{n}\r\n\r\n# Clients\r\nconnected_clients:1\r\n\r\n"
    ))
}

/// (a) THE GUARD. A Moon that rejects the co-location probe `CROSSSLOT` is a
/// multi-shard Moon, and connect MUST refuse — with a message that names the
/// fix (`--shards 1`) and points at the runbook.
#[tokio::test]
async fn crossslot_probe_rejection_fails_connect_naming_the_fix() {
    let (port, _log) =
        spawn_scripted_moon(Script { info: moon_info(), probe: ProbeScript::CrossSlot });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    let Err(StorageError::Backend(msg)) = result else {
        panic!("a multi-shard Moon must fail connect, got {result:?}");
    };
    let lower = msg.to_ascii_lowercase();
    assert!(lower.contains("shard"), "the error must name the problem (sharding); got: {msg}");
    assert!(
        msg.contains("--shards 1"),
        "the error must name the FIX verbatim so it can be pasted into a unit file; got: {msg}"
    );
    assert!(
        msg.contains("docs/operations/external-moon.md"),
        "the error must point at the operator runbook; got: {msg}"
    );
    assert!(
        msg.contains("0008"),
        "the error must cite the RFC that decided this (0008); got: {msg}"
    );
}

/// (a') The guard must not depend on redis-rs classifying the code: a server
/// that phrases the rejection as Moon's TXN wording (`ERR TXN does not support
/// cross-shard writes`, RFC §2.4) is also a multi-shard Moon.
#[tokio::test]
async fn cross_shard_wording_also_fails_connect() {
    // Reuse the CrossSlot script but swap the EXEC reply by scripting a server
    // that answers the TXN-flavoured error instead.
    let (port, _log) =
        spawn_scripted_moon(Script { info: moon_info(), probe: ProbeScript::CrossShardWording });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(
        matches!(result, Err(StorageError::Backend(_))),
        "an `ERR … cross-shard …` rejection must also fail connect, got {result:?}"
    );
}

/// (b) A single-shard Moon executes the probe body and connect proceeds.
#[tokio::test]
async fn single_shard_probe_connects() {
    let (port, _log) =
        spawn_scripted_moon(Script { info: moon_info(), probe: ProbeScript::SingleShard });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(result.is_ok(), "a single-shard Moon must connect: {result:?}");
}

/// (c) REGRESSION GUARD for every existing fake/dev harness in this suite: a
/// server that `+OK`s everything (so the probe reply is not an array) must still
/// connect. The guard fails OPEN on an unrecognizable reply.
#[tokio::test]
async fn unrecognizable_probe_reply_still_connects() {
    let (port, _log) =
        spawn_scripted_moon(Script { info: moon_info(), probe: ProbeScript::AlwaysOk });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(
        result.is_ok(),
        "an unrecognizable probe reply must WARN and continue — otherwise every scripted \
         RESP fake in the suite stops connecting: {result:?}"
    );
}

/// (c') A server with no `MULTI` at all must also still connect.
#[tokio::test]
async fn server_without_multi_support_still_connects() {
    let (port, _log) =
        spawn_scripted_moon(Script { info: InfoScript::Unsupported, probe: ProbeScript::NoMulti });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(
        result.is_ok(),
        "a server that rejects MULTI must WARN and continue, not fail: {result:?}"
    );
}

/// (d) Forward-compatible fast path (upstream ask pilotspace/moon#497): when a
/// future Moon reports `num_shards` in `INFO`, the guard reads it and never
/// needs the probe.
#[tokio::test]
async fn info_num_shards_greater_than_one_fails_connect() {
    let (port, log) = spawn_scripted_moon(Script {
        info: moon_info_with_shards(4),
        // Deliberately a server that would PASS the probe: the verdict must come
        // from INFO alone, not from the canary.
        probe: ProbeScript::SingleShard,
    });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    let Err(StorageError::Backend(msg)) = result else {
        panic!("`num_shards:4` in INFO must fail connect on its own, got {result:?}");
    };
    assert!(msg.contains("--shards 1"), "the error must name the fix; got: {msg}");
    let seen = log.lock().expect("command log poisoned").clone();
    assert!(
        !seen.iter().any(|c| c == "MULTI"),
        "when INFO answers the question the probe must be SKIPPED entirely; saw {seen:?}"
    );
}

/// (d') `num_shards:1` is the same fast path in the other direction: connect
/// proceeds and the probe is skipped (one round-trip saved per connect).
#[tokio::test]
async fn info_num_shards_one_connects_without_probing() {
    let (port, log) = spawn_scripted_moon(Script {
        info: moon_info_with_shards(1),
        // Would FAIL the probe — proving the INFO answer short-circuits it.
        probe: ProbeScript::CrossSlot,
    });
    let url = format!("moon://127.0.0.1:{port}");

    let result = MoonClient::connect_with_dim(&url, 768).await;

    assert!(result.is_ok(), "`num_shards:1` must connect: {result:?}");
    let seen = log.lock().expect("command log poisoned").clone();
    assert!(
        !seen.iter().any(|c| c == "MULTI"),
        "`num_shards:1` must short-circuit the probe; saw {seen:?}"
    );
}

/// (e) RESIDUE PIN. The probe is READ-ONLY: connect must never issue a write
/// command, so there is no canary key to clean up on any path — including the
/// error path. This is the stronger form of "cleans up after itself".
#[tokio::test]
async fn probe_issues_no_write_command() {
    for probe in [ProbeScript::CrossSlot, ProbeScript::SingleShard] {
        let (port, log) = spawn_scripted_moon(Script { info: moon_info(), probe });
        let url = format!("moon://127.0.0.1:{port}");
        let _ = MoonClient::connect_with_dim(&url, 768).await;

        let seen = log.lock().expect("command log poisoned").clone();
        for write in ["SET", "HSET", "DEL", "MSET", "GETSET", "SETEX", "UNLINK", "EXPIRE"] {
            assert!(
                !seen.iter().any(|c| c == write),
                "the shard probe must not write — saw {write} in {seen:?}"
            );
        }
        assert!(
            seen.iter().any(|c| c == "EXISTS"),
            "the probe body must actually have been sent; saw {seen:?}"
        );
    }
}

/// (f) The probe runs exactly ONCE per connect — no per-operation cost.
#[tokio::test]
async fn probe_runs_exactly_once_per_connect() {
    let (port, log) =
        spawn_scripted_moon(Script { info: moon_info(), probe: ProbeScript::SingleShard });
    let url = format!("moon://127.0.0.1:{port}");

    MoonClient::connect_with_dim(&url, 768).await.expect("connect");

    let seen = log.lock().expect("command log poisoned").clone();
    let multis = seen.iter().filter(|c| *c == "MULTI").count();
    assert_eq!(multis, 1, "exactly one co-location probe per connect; saw {seen:?}");
}
