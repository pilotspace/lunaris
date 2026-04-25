//! Plan 05-03 — `POST /v1/recall` contract.
//!
//! Three test functions cover D-03 / D-04 / D-05:
//! - [`happy_path`] — default JSON response (Accept: application/json).
//! - [`sse_streaming`] — SSE branch when `Accept: text/event-stream`.
//! - [`graph_mode_capability_gate`] — `mode: "graph"` returns 200 OR 501
//!   depending on `capabilities().graph_native || graph_pipeline.is_enabled()`.

#![forbid(unsafe_code)]

use std::time::Duration;

use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

/// Default `POST /v1/recall` (Accept: application/json) — returns `Vec<Hit>`.
///
/// Asserts:
/// - response status is `200 OK`,
/// - response body is a JSON array (length is allowed to be 0; an empty
///   conformance corpus is valid by spec).
pub async fn happy_path(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let url = base.join("/v1/recall")?;
    let body = json!({
        "query": "conformance ingest happy_path",
        "k": 5,
        "mode": "semantic",
    });
    let resp = client.post(url).bearer_auth(token).json(&body).send().await?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status == StatusCode::OK,
        "POST /v1/recall expected 200, got {status}; body={payload}"
    );
    anyhow::ensure!(
        payload.is_array(),
        "default recall response should be a JSON array; got {payload}"
    );
    Ok(())
}

/// SSE branch — `Accept: text/event-stream`.
///
/// Asserts:
/// - status `200 OK`,
/// - `Content-Type` contains `text/event-stream`,
/// - the stream emits a terminal `event: done` line within a generous timeout.
///
/// `event: hit` may appear zero or more times depending on whether the
/// conformance corpus has matches; the spec does not mandate at least one hit
/// (an empty result is valid). The terminator is the load-bearing assertion.
pub async fn sse_streaming(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let url = base.join("/v1/recall")?;
    let body = json!({
        "query": "conformance ingest happy_path",
        "k": 3,
        "mode": "semantic",
    });
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "SSE recall expected 200, got {}",
        resp.status()
    );
    let ct =
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    anyhow::ensure!(
        ct.contains("text/event-stream"),
        "SSE recall content-type expected text/event-stream, got {ct:?}"
    );

    let mut buf = Vec::<u8>::new();
    let mut saw_hit = false;
    let mut saw_done = false;
    let mut stream = resp.bytes_stream();

    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => {
                anyhow::bail!(
                    "SSE recall: timed out (10s) waiting for 'event: done' terminator; \
                     buffered {} bytes; saw_hit={saw_hit}",
                    buf.len()
                );
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(b)) => {
                        buf.extend_from_slice(&b);
                        let s = std::str::from_utf8(&buf).unwrap_or("");
                        if s.contains("event: hit") {
                            saw_hit = true;
                        }
                        if s.contains("event: done") {
                            saw_done = true;
                            break;
                        }
                    }
                    Some(Err(e)) => anyhow::bail!("SSE chunk read error: {e}"),
                    None => break,
                }
            }
        }
    }
    anyhow::ensure!(
        saw_done,
        "SSE recall: stream ended without 'event: done' terminator; saw_hit={saw_hit}"
    );
    // saw_hit is informational — a fresh / empty corpus is a valid spec state.
    let _ = saw_hit;
    Ok(())
}

/// `mode: "graph"` capability gate per D-05.
///
/// Plan 05-01's recall handler returns `501 Not Implemented` when neither the
/// backend's `capabilities().graph_native` is true nor the graph pipeline is
/// runtime-enabled. When either is satisfied, the response is `200 OK` with a
/// `Vec<Hit>` body (possibly empty).
///
/// Asserts the response is one of `200 OK` OR `501 Not Implemented` — both
/// are spec-compliant. Anything else (e.g., 500, 404) breaks the contract.
pub async fn graph_mode_capability_gate(
    client: &Client,
    base: &Url,
    token: &str,
) -> anyhow::Result<()> {
    let url = base.join("/v1/recall")?;
    let body = json!({
        "query": "conformance graph mode probe",
        "k": 5,
        "mode": "graph",
    });
    let resp = client.post(url).bearer_auth(token).json(&body).send().await?;
    let status = resp.status();
    anyhow::ensure!(
        status == StatusCode::OK || status == StatusCode::NOT_IMPLEMENTED,
        "graph mode: expected 200 (capability present) or 501 (capability absent), got {status}"
    );
    Ok(())
}
