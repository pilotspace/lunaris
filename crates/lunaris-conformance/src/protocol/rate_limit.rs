//! Plan 05-03 — rate-limit contract (D-08).
//!
//! Subprocess runner sets `--rate-per-second 5 --rate-burst 20`; this test
//! fires up to 30 requests as quickly as possible against `/v1/recall`.
//! Asserts at least one response carries `429 Too Many Requests` plus a
//! `Retry-After` header per spec.
//!
//! The test runs LAST in `run_full_protocol_suite` (per `protocol/mod.rs`)
//! because it intentionally exhausts the tenant's bucket; subsequent tests
//! sharing the same token would flake otherwise.

#![forbid(unsafe_code)]

use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

/// Burst above the per-tenant rate-burst limit; expect `429 + Retry-After`.
pub async fn burst_returns_429(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let url = base.join("/v1/recall")?;
    let body = json!({"query": "rate-limit-burst", "k": 1});

    let mut saw_429 = false;
    let mut saw_retry_after = false;
    // 30 requests gives ample headroom over the runner-configured burst=20
    // even if the per-second refill races with the loop.
    for _ in 0..30u32 {
        let resp = client.post(url.clone()).bearer_auth(token).json(&body).send().await?;
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            if resp.headers().contains_key("retry-after") {
                saw_retry_after = true;
            }
            // Don't break — drain the response body so the connection can be
            // reused; tower-governor's default response is empty so this is
            // cheap.
            let _ = resp.bytes().await;
            break;
        }
        // Drain non-429 bodies to free the conn for reuse.
        let _ = resp.bytes().await;
    }
    anyhow::ensure!(
        saw_429,
        "rate_limit: 30 burst requests did not trigger 429 (rate-burst=20 expected)"
    );
    anyhow::ensure!(saw_retry_after, "rate_limit: 429 response missing Retry-After header");
    Ok(())
}
