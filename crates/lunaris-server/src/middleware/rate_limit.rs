//! Plan 05-01 — per-tenant rate limit via tower-governor (PROTO-05 + D-08).
//!
//! `KeyExtractor` pulls `tenant_id` from `AuthClaims` (attached by
//! `auth_middleware`). Defaults: 60 rps / 120 burst (overridable via
//! [`crate::Config`]).
//!
//! When the request hasn't yet been through `auth_middleware` (i.e., on
//! routes that don't carry auth — currently only `/healthz`, which is NOT
//! layered with this middleware), the `KeyExtractor` returns
//! `GovernorError::UnableToExtractKey` which `tower_governor` maps to a 500.
//! In practice this never fires because the rate-limit layer is mounted
//! INSIDE the auth-protected `/v1` nest (see `lib::build`).

use axum::http::Request;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::KeyExtractor;

use crate::middleware::auth::AuthClaims;

/// Per-tenant key extractor — reads `AuthClaims.tenant` from the request
/// extensions populated by `auth_middleware`.
#[derive(Clone, Debug)]
pub struct TenantKey;

impl KeyExtractor for TenantKey {
    type Key = String;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, tower_governor::GovernorError> {
        req.extensions()
            .get::<AuthClaims>()
            .map(|c| c.scope.as_str().to_string())
            .ok_or(tower_governor::GovernorError::UnableToExtractKey)
    }
}

/// Build a `GovernorLayer` keyed on tenant with the given rps + burst.
///
/// Returns a layer typed for the axum `Body` response so it composes with the
/// rest of the router (the `From<GovernorError> for Response<axum::body::Body>`
/// impl from `tower_governor`'s `axum` feature provides the conversion).
pub fn governor_layer(
    rps: u32,
    burst: u32,
) -> GovernorLayer<
    TenantKey,
    governor::middleware::NoOpMiddleware<governor::clock::QuantaInstant>,
    axum::body::Body,
> {
    let rps = rps.max(1) as u64;
    let burst = burst.max(1);
    let config = GovernorConfigBuilder::default()
        .per_second(rps)
        .burst_size(burst)
        .key_extractor(TenantKey)
        .finish()
        .expect("valid governor config (rps + burst > 0)");
    GovernorLayer::new(config)
}
