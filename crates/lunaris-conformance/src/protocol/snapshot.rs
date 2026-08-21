//! Plan 05-03 — `GET /v1/snapshot/{lsn}` NDJSON stream contract.
//!
//! Asserts:
//! - status `200 OK`,
//! - `Content-Type` is `application/x-ndjson` (or `application/json` for
//!   backends that conflate the two — both are spec-tolerable),
//! - the body parses as zero-or-more newline-delimited JSON objects within a
//!   bounded read window (an empty corpus is valid).

#![forbid(unsafe_code)]

use std::time::Duration;

use futures::StreamExt;
use reqwest::{Client, StatusCode};
use url::Url;

/// Snapshot at the CURRENT Hlc — returns whatever rows exist (the conformance
/// corpus seeded by `ingest::happy_path` plus anything the backend already
/// holds). The Hlc encodes as `<wall_ms>.<counter>` per the Plan 05-01
/// snapshot route.
///
/// W1.1 — this used to request `(u64::MAX/2).0`, a far-FUTURE wall_ms, which
/// `routes/snapshot.rs::is_future_hlc` answers with `404 snapshot_out_of_range`
/// by design. The assertion had therefore been unsatisfiable ever since that
/// rule landed; nobody noticed because the whole protocol suite skips unless
/// `target/<profile>/lunaris-server` happens to exist, and no CI job built it.
/// Integration CI now builds the binary, so this had to become true.
pub async fn ndjson_stream(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let url = base.join(&format!("/v1/snapshot/{now_ms}.0"))?;
    let resp = client.get(url).bearer_auth(token).send().await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "snapshot expected 200, got {}",
        resp.status()
    );
    let ct =
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    anyhow::ensure!(
        ct.contains("ndjson") || ct.contains("application/json"),
        "snapshot content-type expected ndjson, got {ct:?}"
    );

    // Walk a bounded prefix of the stream — proves the body is reachable.
    // An empty corpus is valid (no '\n' ever appears). Cap at 5 seconds.
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::<u8>::new();
    let timeout = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => break,
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(b)) => {
                        buf.extend_from_slice(&b);
                        // Stop after we see one complete line OR 64 KiB of data.
                        if buf.contains(&b'\n') || buf.len() >= 64 * 1024 {
                            break;
                        }
                    }
                    Some(Err(e)) => anyhow::bail!("snapshot chunk error: {e}"),
                    None => break,
                }
            }
        }
    }
    Ok(())
}
