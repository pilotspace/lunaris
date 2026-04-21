//! Plan 05-03 — `POST /v1/ingest` contract test.
//!
//! Asserts the server accepts a well-formed Episode body, returns `200 OK`
//! with `{ "lsn": Lsn, "queue_lag_warn": bool }` per CONTEXT.md D-03.

#![forbid(unsafe_code)]

use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

/// `POST /v1/ingest` happy path — bearer token + minimal Episode body.
///
/// Asserts:
/// - response status is `200 OK`,
/// - response body has `lsn` (Lsn struct: `{ wall_ms, counter }`),
/// - response body has `queue_lag_warn` (boolean).
pub async fn happy_path(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let url = base.join("/v1/ingest")?;
    let body = json!({
        "id": ulid::Ulid::new().to_string(),
        "source": "conformance:protocol-ingest",
        "content": "Hello from the conformance ingest happy_path test.",
        "t_ref": chrono::Utc::now().to_rfc3339(),
        "metadata": {},
    });
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status == StatusCode::OK,
        "POST /v1/ingest expected 200, got {status}; body={payload}"
    );
    anyhow::ensure!(
        payload.get("lsn").is_some(),
        "ingest response missing 'lsn' field; body={payload}"
    );
    let lsn = payload.get("lsn").expect("lsn checked above");
    anyhow::ensure!(
        lsn.get("wall_ms").and_then(|v| v.as_u64()).is_some(),
        "lsn missing 'wall_ms' u64 sub-field; lsn={lsn}"
    );
    anyhow::ensure!(
        lsn.get("counter").and_then(|v| v.as_u64()).is_some(),
        "lsn missing 'counter' u64 sub-field; lsn={lsn}"
    );
    anyhow::ensure!(
        payload.get("queue_lag_warn").and_then(|v| v.as_bool()).is_some(),
        "ingest response missing 'queue_lag_warn' bool field; body={payload}"
    );
    Ok(())
}
