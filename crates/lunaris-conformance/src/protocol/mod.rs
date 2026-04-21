//! Plan 05-03 — protocol suite (PROTO-06). **Body lands in Plan 05-03.**
//!
//! This file is a B-7 forward-compat stub so the crate's `lib.rs`
//! re-export compiles BEFORE Plan 05-03 commits. Plan 05-03 REPLACES this
//! file with per-verb test functions + the full
//! [`run_full_protocol_suite`] body that drives the four MemoryProtocol
//! verbs at a real `lunaris-server` HTTP endpoint.
//!
//! The signature is held stable so third-party harnesses can wire to it
//! today without churning when Plan 05-03 fills in the body.

#![forbid(unsafe_code)]

use reqwest::Client;
use url::Url;

/// Plan 05-03 entry point — exercises the four MemoryProtocol verbs +
/// SSE + auth + rate limit + retrieval modes against the given HTTP
/// endpoint. **Stub body — Plan 05-03 lands the real assertions.**
pub async fn run_full_protocol_suite(
    _client: Client,
    _base_url: Url,
    _token: String,
) -> anyhow::Result<()> {
    eprintln!(
        "protocol::run_full_protocol_suite: STUB — Plan 05-03 lands the body"
    );
    Ok(())
}
