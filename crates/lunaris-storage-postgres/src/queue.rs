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
use sqlx::Row;

use crate::pool::{PgClient, sqlx_err};

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
