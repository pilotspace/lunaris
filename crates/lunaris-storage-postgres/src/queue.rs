//! `publish` / `subscribe` — pgmq.
//!
//! pgmq does not natively partition; we pack the partition into the JSONB envelope so
//! the subscriber can filter at the Lunaris boundary. Phase 2 may add a per-partition
//! queue naming scheme if backpressure demands it.
//!
//! Envelope shape: `{"partition": <u16>, "payload_b64": "<base64>"}`.
//! The base64 helpers are tiny and inline — we deliberately avoid pulling in a
//! `base64` crate dep just for the envelope.
//!
//! ## DoS bound (T-01-04-05)
//!
//! `subscribe` polls `pgmq.read($topic, 30::int, 1)` (visibility timeout 30s, fetch 1)
//! and sleeps 250ms between empty reads. Idle CPU is bounded at ~4 polls/sec.
//! Phase 4 wires real backpressure via VERIFY-06.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::QueueMsg;
use sqlx::{AssertSqlSafe, Row};

use crate::pool::{PgClient, sqlx_err};

/// Plan 04 D-12 + B-11: pgmq queue length.
///
/// Tries `pgmq.queue_length($1)` SQL function first. On SqlState `42883`
/// (`undefined_function` — pgmq.queue_length(text) absent on older pgmq
/// installs) falls back to a direct `count(*)` against the per-topic
/// `pgmq.q_<topic>` table.
///
/// The fallback path interpolates the topic name into the SQL string. To
/// defend against SQL injection (T-04-04-01), the topic name MUST pass
/// [`validate_topic`] BEFORE any `format!()` call. The validator matches
/// `^[A-Za-z_][A-Za-z0-9_]*$` — the standard Postgres safe-identifier
/// shape (mirror of Plan 03's atomic.rs T-01-03-01 contract).
pub(crate) async fn queue_length(
    c: &PgClient,
    topic: &str,
    _partition: u16,
) -> Result<u64, StorageError> {
    // B-11 fix: validate topic name BEFORE any SQL interpolation. The
    // pgmq.queue_length($1) parameterised path doesn't strictly need this,
    // but the fallback `format!("... q_{safe_topic} ...")` does, and
    // running the validator unconditionally keeps the threat model trivially
    // auditable (the validator is the gate; the bind param is defense in
    // depth).
    let safe_topic = validate_topic(topic)?;

    // Path A: try pgmq.queue_length($1) SQL function.
    let try_fn = sqlx::query("SELECT pgmq.queue_length($1) AS depth")
        .bind(safe_topic)
        .fetch_one(&c.pool)
        .await;

    match try_fn {
        Ok(row) => {
            let n: i64 = row
                .try_get("depth")
                .map_err(|e| StorageError::Backend(format!("queue_length depth col: {e}")))?;
            Ok(n.max(0) as u64)
        }
        // B-11 fix: match SqlState code `42883` (undefined_function), NOT
        // brittle string-contains. The pgmq.queue_length() helper is missing
        // on older pgmq installs; fall back to count(*) on the q_<name> table.
        Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("42883") => {
            tracing::debug!(
                "pgmq.queue_length(text) absent (SqlState 42883); falling back to direct count"
            );
            // Safe interpolation: safe_topic already passed validate_topic.
            // Workspace convention (atomic.rs / vector.rs / graph.rs) wraps
            // every dynamically-built SQL string with `AssertSqlSafe(...)` —
            // sqlx 0.9.0-alpha.1's `query()` requires `SqlSafeStr`, and the
            // wrapper is the explicit "this string passed our validator"
            // marker. The validator is the actual gate; this wrapper is the
            // workspace-convention seal that the audit reader greps for.
            let sql = format!("SELECT count(*) AS depth FROM pgmq.q_{safe_topic}");
            let row = sqlx::query(AssertSqlSafe(sql)).fetch_one(&c.pool).await.map_err(sqlx_err)?;
            let n: i64 = row
                .try_get("depth")
                .map_err(|e| StorageError::Backend(format!("queue_length count col: {e}")))?;
            Ok(n.max(0) as u64)
        }
        Err(e) => Err(sqlx_err(e)),
    }
}

/// B-11 fix: validate topic name against the Postgres safe-identifier
/// regex `^[A-Za-z_][A-Za-z0-9_]*$`. Hand-rolled (no `regex` crate dep) —
/// the character set is small enough that a single-pass byte loop is both
/// faster than compiling a regex and avoids pulling a new workspace dep.
///
/// Mirror of the validation contract documented in
/// `lunaris-storage-postgres/src/lib.rs` line 19 ("Caller-validated regex
/// `^[A-Za-z_][A-Za-z0-9_]*$` per Plan 03's T-01-03-01 contract").
fn validate_topic(topic: &str) -> Result<&str, StorageError> {
    let bytes = topic.as_bytes();
    if bytes.is_empty() {
        return Err(StorageError::Backend(format!("invalid pgmq topic: {topic}")));
    }
    // First byte: must be ASCII alpha or underscore.
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(StorageError::Backend(format!("invalid pgmq topic: {topic}")));
    }
    // Subsequent bytes: ASCII alphanumeric or underscore.
    for &b in &bytes[1..] {
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return Err(StorageError::Backend(format!("invalid pgmq topic: {topic}")));
        }
    }
    Ok(topic)
}

pub(crate) async fn publish(
    c: &PgClient,
    topic: &str,
    partition: u16,
    payload: Bytes,
) -> Result<u64, StorageError> {
    let env = serde_json::json!({
        "partition": partition,
        "payload_b64": base64_encode(&payload),
    });
    let row = sqlx::query("SELECT pgmq.send($1, $2::jsonb) AS id")
        .bind(topic)
        .bind(env)
        .fetch_one(&c.pool)
        .await
        .map_err(sqlx_err)?;
    let id: i64 = row.try_get("id").unwrap_or(0);
    Ok(id as u64)
}

pub(crate) async fn subscribe(
    client: PgClient,
    _group: &str,
    topic: &str,
    partition: u16,
) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
    let topic_s = topic.to_string();

    let stream = stream::unfold((client, topic_s, partition), |(c, t, p)| async move {
        loop {
            let row = sqlx::query("SELECT msg_id, message FROM pgmq.read($1, 30::int, 1)")
                .bind(t.as_str())
                .fetch_optional(&c.pool)
                .await;

            match row {
                Err(e) => return Some((Err(sqlx_err(e)), (c, t, p))),
                Ok(None) => {
                    // No message — back off briefly and try again. Bounded at ~4 polls/sec.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                Ok(Some(r)) => {
                    let msg_id: i64 = r.try_get("msg_id").unwrap_or(0);
                    let env: serde_json::Value =
                        r.try_get("message").unwrap_or(serde_json::Value::Null);
                    let payload_b64 = env.get("payload_b64").and_then(|x| x.as_str()).unwrap_or("");
                    let payload = base64_decode(payload_b64);
                    let env_partition =
                        env.get("partition").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                    if env_partition != p {
                        // Not for this partition — drop and continue polling.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    return Some((
                        Ok(QueueMsg {
                            topic: t.clone(),
                            partition: p,
                            offset: msg_id as u64,
                            payload: Bytes::from(payload),
                        }),
                        (c, t, p),
                    ));
                }
            }
        }
    });

    Ok(stream.boxed())
}

// Tiny base64 helpers — avoid a crate dep just for the queue envelope.

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[(n >> 18 & 0x3F) as usize] as char);
        out.push(ALPHA[(n >> 12 & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHA[(n >> 6 & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHA[(n & 0x3F) as usize] as char } else { '=' });
    }
    out
}

fn base64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> i32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as i32,
            b'a'..=b'z' => (c - b'a' + 26) as i32,
            b'0'..=b'9' => (c - b'0' + 52) as i32,
            b'+' => 62,
            b'/' => 63,
            _ => -1,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        let v0 = val(chunk[0]);
        let v1 = val(chunk[1]);
        let v2 = if chunk.len() > 2 { val(chunk[2]) } else { -1 };
        let v3 = if chunk.len() > 3 { val(chunk[3]) } else { -1 };
        if v0 < 0 || v1 < 0 {
            continue;
        }
        out.push(((v0 << 2) | (v1 >> 4)) as u8);
        if v2 >= 0 {
            out.push((((v1 & 0x0F) << 4) | (v2 >> 2)) as u8);
        }
        if v3 >= 0 {
            out.push((((v2 & 0x03) << 6) | v3) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        let cases: &[&[u8]] =
            &[b"", b"A", b"AB", b"ABC", b"hello, world", b"\x00\xff\x10\x20\x30\x40"];
        for c in cases {
            let enc = base64_encode(c);
            let dec = base64_decode(&enc);
            assert_eq!(&dec[..], &c[..], "roundtrip failed for {c:?}");
        }
    }

    #[test]
    fn base64_empty_is_empty_string() {
        assert_eq!(base64_encode(&[]), "");
        assert_eq!(base64_decode(""), Vec::<u8>::new());
    }
}

#[cfg(test)]
mod queue_length_tests {
    //! B-11 unit tests — validate the topic-name regex BEFORE any SQL hits
    //! Postgres. These run on every `cargo test --workspace --lib` (no
    //! pg-it gate) because they exercise the pure validator only.

    use super::*;

    #[test]
    fn valid_table_names_accepted() {
        assert!(validate_topic("__lunaris_verify__").is_ok());
        assert!(validate_topic("__lunaris_consolidate__").is_ok());
        assert!(validate_topic("__lunaris_audit__").is_ok());
        assert!(validate_topic("topic1").is_ok());
        assert!(validate_topic("_topic").is_ok());
        assert!(validate_topic("Topic_With_Mixed_Case").is_ok());
        assert!(validate_topic("a").is_ok());
        assert!(validate_topic("_").is_ok());
    }

    #[test]
    fn injection_attempts_rejected() {
        assert!(validate_topic("").is_err(), "empty must reject");
        assert!(validate_topic("1invalid").is_err(), "leading digit must reject");
        assert!(validate_topic("helios:fs/").is_err(), "colon + slash must reject");
        assert!(validate_topic("foo' OR '1").is_err(), "single-quote injection must reject");
        assert!(
            validate_topic("foo\"; DROP TABLE x; --").is_err(),
            "double-quote + statement injection must reject"
        );
        assert!(validate_topic("foo bar").is_err(), "space must reject");
        assert!(validate_topic("foo-bar").is_err(), "hyphen must reject");
        assert!(validate_topic("foo.bar").is_err(), "dot must reject");
        // Unicode + non-ASCII must reject (regex is ASCII-only).
        assert!(validate_topic("föo").is_err(), "non-ASCII must reject");
    }

    /// Regression guard: the validator MUST reject empty input even though
    /// `bytes.is_empty()` early returns — keep the explicit assertion so a
    /// future refactor can't silently allow it through.
    #[test]
    fn empty_topic_rejected() {
        assert!(validate_topic("").is_err());
    }
}
