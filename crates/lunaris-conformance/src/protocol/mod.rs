//! Plan 05-03 — protocol suite (PROTO-06).
//!
//! Per-verb HTTP test functions parameterized over `(reqwest::Client, base_url, token)`.
//! Run via [`run_full_protocol_suite`] which fans out to every per-verb module.
//!
//! Tests fire HTTP requests at a running `lunaris-server` (Plan 05-01 binary)
//! and assert MemoryProtocol 0.1 contract per CONTEXT.md D-03 / D-04 / D-05 /
//! D-07 / D-08 / D-21.
//!
//! The companion [`tests/run_protocol_lunaris_server.rs`](../../tests/run_protocol_lunaris_server.rs)
//! (Task 2 of this plan) spawns the server as a subprocess + parses
//! `LISTENING_ON <addr>` from stderr + runs this suite against it.
//!
//! ## D-11 contract
//!
//! Caller provides:
//! - a pre-validated `reqwest::Client` (timeouts, TLS configured upstream),
//! - a `base_url` pointing at the server root (e.g., `http://127.0.0.1:54321`),
//! - a bearer token with FULL scopes (`["ingest", "recall", "forget"]`) for the
//!   happy-path tests.
//!
//! The auth + scope-isolation tests use the additional `tok-ingest` token that
//! the subprocess runner writes into the tokens-file (only `ingest` scope) —
//! they hit the server directly with that token instead of the caller-supplied
//! one. See `auth::wrong_scope_returns_403`.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use reqwest::Client;
use url::Url;

pub mod auth;
pub mod forget;
pub mod ingest;
pub mod rate_limit;
pub mod recall;
pub mod snapshot;

/// Run every protocol-suite test against a running `lunaris-server`.
///
/// Tests are sequenced so the corpus seeded by `ingest::happy_path` is visible
/// to subsequent recall / forget / snapshot tests; rate_limit runs LAST because
/// it intentionally bursts above the per-tenant limit.
pub async fn run_full_protocol_suite(
    client: Client,
    base_url: Url,
    token: String,
) -> anyhow::Result<()> {
    ingest::happy_path(&client, &base_url, &token).await?;
    recall::happy_path(&client, &base_url, &token).await?;
    recall::sse_streaming(&client, &base_url, &token).await?;
    recall::graph_mode_capability_gate(&client, &base_url, &token).await?;
    forget::id_target(&client, &base_url, &token).await?;
    forget::two_step_hard_delete(&client, &base_url, &token).await?;
    snapshot::ndjson_stream(&client, &base_url, &token).await?;
    auth::missing_token_returns_401(&client, &base_url).await?;
    auth::wrong_scope_returns_403(&client, &base_url).await?;
    // Rate-limit MUST be last — it bursts above the limit and leaves the
    // tenant's bucket exhausted briefly.
    rate_limit::burst_returns_429(&client, &base_url, &token).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial smoke test asserting [`run_full_protocol_suite`] is callable
    /// (the body returns an error when no server listens, but the function
    /// signature must remain stable for third-party harnesses).
    #[tokio::test]
    async fn run_full_protocol_suite_is_callable() {
        let client = Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .expect("build client");
        let base = Url::parse("http://127.0.0.1:1").expect("parse url");
        // Expect Err — port 1 has nothing listening; we only assert the call
        // compiles + returns a `Result`.
        let _ = run_full_protocol_suite(client, base, "tok-test".to_string()).await;
    }
}
