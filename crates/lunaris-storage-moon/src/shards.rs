//! Connect-time **single-shard guard** — refuse a sharded Moon at startup
//! instead of failing mid-ingest (0.7.0 task 22, RFC 0008 §6 Option C).
//!
//! ## Why a guard at all
//!
//! `docs/rfcs/0008-sharded-moon-ingest.md` settled the question with evidence
//! from the pinned `vendor/moon` source plus a live `--shards 1/2/4` probe.
//! Sharding breaks Lunaris on BOTH sides, independently:
//!
//! * **Write side (RFC §2.3).** Moon's `TXN.*` guard is not "all keys must
//!   co-locate" — it is "all keys must land on **the connection's own shard**"
//!   (`vendor/moon/src/server/conn/handler_monoio/mod.rs:2063-2069`). Connections
//!   are assigned round-robin (`listener.rs:453-459`) with no client control, so
//!   ingest fails on roughly `1 - 1/N` of connections, non-deterministically.
//!   Hash-tagging the keyspace does NOT fix this (the correction RFC 0008 makes
//!   to the prose previously carried in `docs/operations/external-moon.md` §5).
//! * **Read side (RFC §1.3).** `FT.NAVIGATE` — issued on the recall path by
//!   [`crate::navigate`] — does not scatter-gather
//!   (`vendor/moon/src/server/conn/handler_monoio/ft.rs:540-546`, a bare
//!   `with_shard`). A Navigate for a scope owned by another shard returns
//!   **empty, with no error**. Silently losing graph recall is strictly worse
//!   than a loud failure, which is why fixing only the write side was rejected
//!   (RFC §6 Option A).
//!
//! Without this guard the mismatch first appears inside a half-applied ingest
//! `TXN`, minutes or hours after startup. With it, the process refuses to come
//! up and names the fix.
//!
//! ## The probe
//!
//! Moon exposes **no shard count**: `INFO` (all sections), `CONFIG GET *shard*`,
//! `CLIENT INFO` and `CLUSTER KEYSLOT` were all checked live and expose nothing
//! (RFC §2.3). The upstream ask to add one is
//! [pilotspace/moon#497](https://github.com/pilotspace/moon/issues/497), and
//! [`shard_count_from_info`] is already wired for it — the day a Moon reports
//! `num_shards`, the guard reads it and skips the probe entirely.
//!
//! Until then the guard uses RFC §2.2's co-location canary, narrowed to its
//! cheapest correct form — a **read-only** `MULTI` body over keys that would
//! hash to different shards:
//!
//! ```text
//! MULTI
//! EXISTS lunaris:__shardprobe__:canary:0 … :63    (ONE command, 64 keys)
//! EXEC
//! ```
//!
//! * `num_shards > 1` → `analyze_txn_locality`
//!   (`vendor/moon/src/server/conn/shared.rs:800-874`) sees keys owned by more
//!   than one shard and `EXEC` is rejected `CROSSSLOT Keys in MULTI/EXEC don't
//!   hash to the same shard` (`handler_monoio/write.rs:833-838`).
//! * `num_shards == 1` → the body executes and `EXEC` returns its array.
//!
//! ### Why `MULTI/EXEC` and not `TXN.*`
//!
//! `TXN.*`'s rejection depends on which shard the *connection* landed on, so a
//! probe built on it would be non-deterministic (RFC §2.3 — the observed sweep
//! rejected 11 of 16 tags on one connection and would reject a different 11 on
//! another). `MULTI/EXEC` classifies the whole body up front against
//! `num_shards`, so its verdict is a property of the SERVER, not of the
//! connection. That is exactly the question being asked.
//!
//! ### Why read-only, and why 64 keys
//!
//! `command_keys` (`vendor/moon/src/tracking/invalidation.rs:132-159`) reads the
//! command-metadata key spec, and `EXISTS` declares `first_key: 1, last_key: -1`
//! (`vendor/moon/src/command/metadata.rs:291`) — so a single `EXISTS` with N
//! arguments contributes all N keys to the locality analysis. Reads and writes
//! are classified identically (the guard is pure key hashing), so a read-only
//! body has the same discriminating power while:
//!
//! * writing nothing on ANY path — there is no canary key to clean up, not even
//!   on the error path (pinned by
//!   `tests/multishard_failfast.rs::probe_issues_no_write_command`), and nothing
//!   for `SCAN MATCH lunaris:*` in [`crate::scopes`] to trip over; and
//! * working against a read-only replica, which a write canary would fail.
//!
//! Detection needs only that the probe keys do not ALL collapse onto one shard.
//! With [`PROBE_KEY_COUNT`] = 64 pseudo-random keys, the worst case is
//! `num_shards == 2`, where a false negative needs all 64 keys to agree —
//! `2^-63`. That argument deliberately does not depend on Moon's specific hash
//! (`xxh64`, seed 0, `dispatch.rs:169-173`), so the guard survives an upstream
//! hash change. There is no false-POSITIVE risk in the other direction: at
//! `--shards 1`, `key_to_shard` returns `0` for every key by construction.
//!
//! ## Policy
//!
//! | observation                                 | outcome                        |
//! |---------------------------------------------|--------------------------------|
//! | `INFO` reports `num_shards` > 1             | HARD ERROR at connect          |
//! | `INFO` reports `num_shards` == 1            | ok, probe SKIPPED              |
//! | probe rejected `CROSSSLOT` / `cross-shard`  | HARD ERROR at connect          |
//! | probe executed (array reply)                | `debug!`, connect proceeds     |
//! | probe answered with any other server error  | `warn!` once, connect proceeds |
//! | probe reply shape not understood            | `warn!` once, connect proceeds |
//! | probe hit a transport fault (timeout / IO)  | HARD ERROR (fail closed)       |
//!
//! The fail-open rows mirror the version handshake's philosophy
//! ([`crate::version`]): a scripted RESP fake, a plain Redis, or a cut-down dev
//! build must stay dialable — this guard's job is to explain a real mismatch,
//! not to police what a developer may point at. Failing closed on a transport
//! fault is equally deliberate: the connection is already broken, and warning
//! past it just defers the same failure by one command.
//!
//! ## Cost
//!
//! ONE round-trip, ONCE per `connect` — zero if `INFO` answered the question.
//! Nothing on the hot path calls it. It is deliberately NOT re-run on reconnect:
//! `redis::aio::ConnectionManager` (see [`crate::retry`]) re-dials the SAME
//! endpoint, and a server cannot change its shard count beneath a live handle
//! (that requires an AOF migration and a restart — `vendor/moon/src/config.rs`
//! `--migrate-aof-shards`).

/// Reserved partition for the co-location probe. Valid under the `Scope`
/// alphabet (`[A-Za-z0-9_\-.]{1,128}`) and cannot collide with a tenant scope
/// minted from a bearer token unless an operator deliberately names a tenant
/// `__shardprobe__` — the same reservation convention as `lunaris-server`'s
/// readiness canary (`crates/lunaris-server/src/readiness.rs`, `__health__`).
///
/// Nothing is ever written under it (the probe is read-only), so this is a
/// namespace reservation for the *key names on the wire*, not for stored data.
pub const PROBE_SCOPE: &str = "__shardprobe__";

/// How many distinct keys the co-location probe spans.
///
/// See the module docs: detection needs the keys not to all collapse onto one
/// shard, and 64 keys bound the worst case (`num_shards == 2`) at `2^-63`
/// without assuming anything about Moon's hash function. All 64 ride ONE
/// `EXISTS` command, so the cost is one round-trip regardless.
pub const PROBE_KEY_COUNT: usize = 64;

/// `INFO` field names that could carry Moon's shard count, most specific first.
///
/// **None of these exist today** — RFC 0008 §2.3 checked `INFO` (all sections),
/// `CONFIG GET *shard*`, `CLIENT INFO` and `CLUSTER KEYSLOT` against a live
/// Moon 0.8.1 and found nothing. This list is the forward-compatible seam for
/// upstream ask pilotspace/moon#497: whichever name lands, the guard reads it
/// and skips the probe.
pub const SHARD_COUNT_FIELDS: [&str; 4] = ["num_shards", "shard_count", "moon_shards", "shards"];

/// The `i`th probe key. Deliberately NOT built with
/// `lunaris_core::keyspace::*_key`: those mint `lunaris:{scope}:{kind}:{ulid}`
/// for *primitives*, and the probe wants a fixed, enumerable, never-written set.
#[must_use]
pub fn probe_key(i: usize) -> String {
    format!("lunaris:{PROBE_SCOPE}:canary:{i}")
}

/// The full probe key set — [`PROBE_KEY_COUNT`] distinct keys under
/// [`PROBE_SCOPE`].
#[must_use]
pub fn probe_keys() -> Vec<String> {
    (0..PROBE_KEY_COUNT).map(probe_key).collect()
}

/// What the guard concluded about the server's shard topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardTopology {
    /// The server is single-shard (or is co-locating everything, which is the
    /// same thing from Lunaris' side). Carries how we know.
    Single(String),
    /// The server is SHARDED. Carries the observation that proved it, which the
    /// connect error quotes back to the operator.
    Multi(String),
    /// Could not tell. Carries a human-readable account of what WAS seen, so the
    /// warning names the actual observation rather than a generic "unknown".
    Unknown(String),
}

/// Read a shard count out of an `INFO` body, if the server publishes one.
///
/// Returns `None` when no [`SHARD_COUNT_FIELDS`] entry is present or parses —
/// which is the case for every Moon shipped to date (see that constant's doc).
/// A reported `0` also yields `None`: `--shards 0` is Moon's *auto-detect*
/// request (`vendor/moon/src/config.rs`), not a resolved count, so a server
/// echoing it back has told us nothing.
#[must_use]
pub fn shard_count_from_info(info: &str) -> Option<u64> {
    SHARD_COUNT_FIELDS
        .iter()
        .find_map(|name| crate::version::info_field(info, name)?.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
}

/// Classify an `INFO` body. `None` means "the reply said nothing about shards"
/// — run the probe.
#[must_use]
pub fn classify_info(info: &str) -> Option<ShardTopology> {
    let n = shard_count_from_info(info)?;
    Some(if n > 1 {
        ShardTopology::Multi(format!("the server's `INFO` reports {n} shards"))
    } else {
        ShardTopology::Single("the server's `INFO` reports a single shard".to_string())
    })
}

/// Classify a server-side rejection of the probe.
///
/// Split from the `redis::RedisError` adapter ([`classify_probe_error`]) so the
/// policy is unit-testable without constructing driver types.
///
/// Two independent signatures count as proof of sharding, because Moon has two
/// wordings for the same fact (RFC §2.3, §2.4):
///
/// * `CROSSSLOT Keys in MULTI/EXEC don't hash to the same shard`
///   (`handler_monoio/write.rs:835-837`) — what this probe expects; and
/// * `ERR TXN does not support cross-shard writes …`
///   (`ERR_TXN_CROSS_SHARD`) — the `TXN.*` guard's phrasing, which a differently
///   shaped server could answer with.
///
/// Both are *only* producible by a multi-shard server, so matching either is
/// sound. Everything else is inconclusive, never a failure.
#[must_use]
pub fn classify_error_text(code: Option<&str>, message: &str) -> ShardTopology {
    let lower = message.to_ascii_lowercase();
    let crossslot = code.is_some_and(|c| c.eq_ignore_ascii_case("CROSSSLOT"))
        || lower.contains("crossslot")
        || lower.contains("cross-slot");
    if crossslot || lower.contains("cross-shard") || lower.contains("cross shard") {
        ShardTopology::Multi(format!("the server rejected a co-location probe: {message}"))
    } else {
        ShardTopology::Unknown(format!("the server rejected the co-location probe ({message})"))
    }
}

/// [`classify_error_text`] over a driver error.
#[must_use]
pub fn classify_probe_error(err: &redis::RedisError) -> ShardTopology {
    classify_error_text(err.code(), &err.to_string())
}

/// Classify the probe's reply frames (`[MULTI ack, QUEUED ack, EXEC reply]`).
///
/// Only an ARRAY in the `EXEC` slot proves the body actually executed, which is
/// what makes it evidence of co-location. Anything else — a bare `+OK` from a
/// fake that acks everything, a nil, a short reply — is inconclusive and warned
/// past, which is what keeps the scripted RESP fakes in this crate's test suite
/// connectable.
#[must_use]
pub fn classify_probe_reply(replies: &[redis::Value]) -> ShardTopology {
    match replies.last() {
        Some(redis::Value::Array(_)) => ShardTopology::Single(format!(
            "a co-location probe spanning {PROBE_KEY_COUNT} keys executed as one MULTI/EXEC \
             body, so the server routes every key to the same shard"
        )),
        Some(other) => ShardTopology::Unknown(format!(
            "the co-location probe's EXEC returned {other:?}, not a transaction result array"
        )),
        None => {
            ShardTopology::Unknown("the co-location probe returned no frames at all".to_string())
        }
    }
}

/// Run the co-location probe on an established connection and classify the
/// answer.
///
/// This is the whole wire-level guard, factored out of
/// `MoonClient::single_shard_guard` so a live harness can exercise the EXACT
/// production path against a real `--shards N` Moon without going through
/// `connect` (and its version gate). See the module docs for the design.
///
/// `Err` is reserved for TRANSPORT faults ([`probe_failure_is_fatal`]) — the
/// caller must fail closed on those. Every server-side answer, including a
/// rejection, comes back as an `Ok(ShardTopology)` verdict.
///
/// ## Why a hand-spelled `MULTI` … `EXEC` and not `redis::pipe().atomic()`
///
/// In redis-rs transaction mode the driver pops the `EXEC` reply and *demands*
/// an array (`redis-1.2.0/src/pipeline.rs:238-252`), so Moon's `-CROSSSLOT`
/// frame would be flattened into a generic `UnexpectedReturnType` — destroying
/// the one signal this guard exists to read. In plain pipeline mode the frames
/// come back intact and `extract_error` preserves the code. It is still ONE
/// contiguous write on the multiplexed connection, so no other task's command
/// can interleave between `MULTI` and `EXEC`.
pub async fn probe_shard_topology<C>(conn: &mut C) -> Result<ShardTopology, redis::RedisError>
where
    C: redis::aio::ConnectionLike,
{
    let mut body = redis::cmd("EXISTS");
    for key in probe_keys() {
        body.arg(key);
    }
    let mut probe = redis::pipe();
    probe.cmd("MULTI");
    probe.add_command(body);
    probe.cmd("EXEC");

    match probe.query_async::<Vec<redis::Value>>(conn).await {
        Ok(replies) => Ok(classify_probe_reply(&replies)),
        // Transport fault — the connection is unusable; let the caller fail closed.
        Err(e) if probe_failure_is_fatal(&e) => Err(e),
        // The server answered, it just answered "no". Only a cross-shard
        // rejection is a verdict; everything else is inconclusive.
        Err(e) => Ok(classify_probe_error(&e)),
    }
}

/// Should a failed probe abort the connect?
///
/// `true` for transport-level faults (timeout, dropped socket, IO error) — the
/// connection is unusable and continuing only re-fails one command later. Same
/// predicate, and the same reasoning, as the version handshake's; delegated so
/// there is exactly one definition of "the wire broke" in this crate.
#[must_use]
pub fn probe_failure_is_fatal(err: &redis::RedisError) -> bool {
    crate::version::info_probe_failure_is_fatal(err)
}

/// The connect-time error text for a detected multi-shard server.
///
/// Names the fix verbatim (`--shards 1`), calls out the container-image footgun
/// that produces most sharded deployments by accident, and cites both the
/// operator runbook and the RFC that decided this.
#[must_use]
pub fn multi_shard_error(host: &str, port: u16, detail: &str) -> String {
    format!(
        "moon: MULTI-SHARD server detected at {host}:{port} — {detail}. \
         Lunaris supports a SINGLE-SHARD Moon only, and this is a correctness \
         requirement, not a preference: on a sharded Moon (a) ingest fails \
         mid-transaction, because Moon's TXN guard rejects any write whose key is not \
         owned by the CONNECTION's own shard and connections are assigned \
         round-robin, and (b) graph recall silently returns EMPTY, because \
         FT.NAVIGATE answers from the connection's shard only and never \
         scatter-gathers. \
         To proceed: restart Moon with `--shards 1` (that is the BINARY default; \
         the published container image's CMD passes `--shards 0` = auto-detect \
         = one shard per core, so pin `--shards 1` explicitly in your compose / \
         helm / systemd arguments), then reconnect. Changing the shard count of an \
         existing data directory is a Moon-side AOF migration — see \
         docs/operations/external-moon.md §5. \
         Full evidence and the deferred-sharding decision: \
         docs/rfcs/0008-sharded-moon-ingest.md."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_keys_are_distinct_and_scoped_to_the_reserved_partition() {
        let keys = probe_keys();
        assert_eq!(keys.len(), PROBE_KEY_COUNT);
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), PROBE_KEY_COUNT, "duplicate keys would weaken the probe");
        for k in &keys {
            assert!(
                k.starts_with(&format!("lunaris:{PROBE_SCOPE}:")),
                "probe keys must live under the reserved partition: {k}"
            );
        }
    }

    #[test]
    fn probe_keys_carry_no_hash_tag() {
        // A `{…}` anywhere in the key would make Moon hash the TAG instead of
        // the key (`vendor/moon/src/shard/dispatch.rs:169-173`), collapsing all
        // 64 probe keys onto one shard and turning the guard into a no-op.
        for k in probe_keys() {
            assert!(!k.contains('{') && !k.contains('}'), "probe key must not be hash-tagged: {k}");
        }
    }

    #[test]
    fn info_shard_count_is_read_when_present() {
        let info = "# Server\r\nmoon_version:0.9.0\r\nnum_shards:4\r\n";
        assert_eq!(shard_count_from_info(info), Some(4));
        assert!(matches!(classify_info(info), Some(ShardTopology::Multi(_))));
    }

    #[test]
    fn info_shard_count_of_one_is_single() {
        let info = "# Server\r\nmoon_version:0.9.0\r\nnum_shards:1\r\n";
        assert!(matches!(classify_info(info), Some(ShardTopology::Single(_))));
    }

    #[test]
    fn todays_moon_info_says_nothing_about_shards() {
        // The exact block Moon emits today (`connection.rs:176-180`). RFC 0008
        // §2.3: there is no shard count anywhere in it.
        let info = "# Server\r\nredis_version:7.4.0\r\nmoon_version:0.8.5\r\nmoon:true\r\n";
        assert_eq!(shard_count_from_info(info), None);
        assert_eq!(classify_info(info), None, "no field ⇒ fall through to the probe");
    }

    #[test]
    fn auto_detect_zero_is_not_a_resolved_count() {
        let info = "# Server\r\nnum_shards:0\r\n";
        assert_eq!(shard_count_from_info(info), None);
        assert_eq!(classify_info(info), None);
    }

    #[test]
    fn crossslot_is_proof_of_sharding_by_code_or_by_text() {
        for (code, msg) in [
            (Some("CROSSSLOT"), "An error was signalled by the server - CrossSlot: whatever"),
            (None, "CROSSSLOT Keys in MULTI/EXEC don't hash to the same shard"),
            (Some("ERR"), "ERR TXN does not support cross-shard writes -- use hash tags"),
        ] {
            assert!(
                matches!(classify_error_text(code, msg), ShardTopology::Multi(_)),
                "must be read as multi-shard: {msg}"
            );
        }
    }

    #[test]
    fn every_other_server_error_is_inconclusive_never_fatal() {
        for (code, msg) in [
            (Some("ERR"), "ERR unknown command 'MULTI'"),
            (Some("NOPERM"), "NOPERM this user has no permissions"),
            (Some("ERR"), "ERR EXEC without MULTI"),
            (None, "Invalid response when parsing multi response"),
        ] {
            assert!(
                matches!(classify_error_text(code, msg), ShardTopology::Unknown(_)),
                "must be inconclusive (fail-open), not a verdict: {msg}"
            );
        }
    }

    #[test]
    fn only_an_array_exec_reply_proves_co_location() {
        assert!(matches!(
            classify_probe_reply(&[
                redis::Value::Okay,
                redis::Value::Okay,
                redis::Value::Array(vec![redis::Value::Int(0)]),
            ]),
            ShardTopology::Single(_)
        ));
        // The `+OK`-to-everything fake shape: inconclusive, NOT single.
        assert!(matches!(
            classify_probe_reply(&[redis::Value::Okay, redis::Value::Okay, redis::Value::Okay]),
            ShardTopology::Unknown(_)
        ));
        assert!(matches!(classify_probe_reply(&[]), ShardTopology::Unknown(_)));
    }

    #[test]
    fn the_multi_shard_error_is_actionable() {
        let msg = multi_shard_error("moon.internal", 6380, "the probe was rejected CROSSSLOT");
        assert!(msg.contains("moon.internal"), "must name the endpoint: {msg}");
        assert!(msg.contains("6380"), "must name the port: {msg}");
        assert!(msg.contains("--shards 1"), "must name the fix verbatim: {msg}");
        assert!(msg.contains("docs/operations/external-moon.md"), "must cite the runbook: {msg}");
        assert!(msg.contains("0008"), "must cite the RFC: {msg}");
    }
}
