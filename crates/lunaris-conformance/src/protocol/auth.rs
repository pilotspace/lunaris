//! Plan 05-03 — auth contract (D-07).
//!
//! Two test functions:
//! - [`missing_token_returns_401`] — no `Authorization` header → `401`.
//! - [`wrong_scope_returns_403`] — token present but lacks the required scope
//!   for the route → `403`. Uses the test-only `tok-ingest` token (scope
//!   `["ingest"]`) on a `/v1/recall` route which requires the `recall` scope.

#![forbid(unsafe_code)]

use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

/// No `Authorization` header → `401 Unauthorized`.
pub async fn missing_token_returns_401(client: &Client, base: &Url) -> anyhow::Result<()> {
    let url = base.join("/v1/ingest")?;
    let body = json!({
        "id": ulid::Ulid::new().to_string(),
        "source": "conformance:auth-missing-token",
        "content": "missing-token-probe",
    });
    let resp = client.post(url).json(&body).send().await?;
    anyhow::ensure!(
        resp.status() == StatusCode::UNAUTHORIZED,
        "missing token expected 401, got {}",
        resp.status()
    );
    Ok(())
}

/// Token present but lacking the required scope → `403 Forbidden`.
///
/// Uses the test-only `tok-ingest` bearer token (scope `["ingest"]`) on the
/// `/v1/recall` route (which requires the `recall` scope per Plan 05-01
/// `lib.rs::scoped_auth`).
///
/// The subprocess runner (`crates/lunaris-conformance/tests/run_protocol_lunaris_server.rs`)
/// writes this token into the `--tokens-file` JSON when it spawns the server.
pub async fn wrong_scope_returns_403(client: &Client, base: &Url) -> anyhow::Result<()> {
    let url = base.join("/v1/recall")?;
    let body = json!({"query": "wrong-scope-probe", "k": 1});
    let resp = client.post(url).bearer_auth("tok-ingest").json(&body).send().await?;
    anyhow::ensure!(
        resp.status() == StatusCode::FORBIDDEN,
        "wrong-scope expected 403, got {}",
        resp.status()
    );
    Ok(())
}
