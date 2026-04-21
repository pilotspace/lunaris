//! Plan 05-01 — `lunaris-server` crate.
//!
//! MemoryProtocol 0.1 HTTP+SSE wrapper around `lunaris::Lunaris`. Per
//! CONTEXT.md D-01..D-09 + ROADMAP Phase 5 success criterion #1.
//!
//! Module map:
//! - [`config`] — clap `Config` struct (D-01..D-09 flags + matching env vars).
//! - [`state`] — `AppState { lunaris: Arc<Lunaris>, tokens, runtime_flags }`.
//! - [`dto`] — JSON wire DTOs (`RecallRequest`, `IngestResponse`, `RetrievalMode`).
//! - [`shutdown`] — `tokio::sync::Notify` graceful-shutdown wrapper.
//! - [`routes`] — per-verb handler modules (`ingest`, `recall`, `forget`, `snapshot`, `healthz`).
//! - [`middleware`] — `auth`, `rate_limit`, `cors`, `error`.
//!
//! ## Construction
//!
//! ```ignore
//! let cfg = Config::parse();
//! let lunaris = Arc::new(lunaris::Lunaris::open(&cfg.storage).await?);
//! let app = lunaris_server::build(cfg, lunaris);
//! axum::serve(listener, app).await?;
//! ```

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

pub mod config;
pub mod dto;
pub mod middleware;
pub mod routes;
pub mod shutdown;
pub mod state;

pub use config::Config;
pub use shutdown::Shutdown;
pub use state::AppState;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::middleware::auth::{RequiredScope, auth_middleware};

/// Build the axum `Router` for the lunaris-server binary.
///
/// Wires every middleware layer + route registration per CONTEXT.md D-01..D-09:
/// - `/v1/{ingest,recall,forget,snapshot/:lsn}` gated by Bearer auth + per-tenant
///   rate limit (PROTO-05 + D-07 + D-08).
/// - `/healthz` open (no auth).
/// - Outer CORS layer per D-09.
///
/// ## Layer order
///
/// axum's `.layer(X)` adds X to the OUTSIDE of the existing stack — so the
/// LAST `.layer(...)` call runs FIRST on the request path. We need:
///
///   request → CORS → rate-limit → enrich(scope) → auth → handler
///
/// To get `enrich → auth` ordering on the request, we wire BOTH as a single
/// per-route closure that calls them in the correct sequence — `route_layer`
/// vs `.layer` ordering quirks aren't ergonomic for two-step composition. The
/// outer `.layer` wires the rate-limit layer (which is outside the per-route
/// auth check, so an un-authenticated request still gets rate-limited at the
/// IP-aware fallback path inside the governor key extractor).
pub fn build(cfg: Config, lunaris: Arc<lunaris::Lunaris>) -> Router {
    let tokens = load_tokens(&cfg.tokens_file).unwrap_or_default();
    let state = AppState::new(lunaris, tokens);

    // Per-route stacking. `route_layer` calls add to the OUTSIDE of the
    // existing per-route stack — so the LAST `.route_layer` invocation runs
    // FIRST on the request. Order we want on each authenticated route:
    //
    //   request → scoped_auth (auth + scope check) → rate_limit (per-tenant)
    //           → handler
    //
    // We therefore add rate_limit FIRST (closer to the handler) and
    // scoped_auth SECOND (outer, runs first on request). This guarantees the
    // governor's `TenantKey::extract` always sees `AuthClaims` in the
    // extensions populated by `auth_middleware`.
    let rate_limit = middleware::rate_limit::governor_layer(cfg.rate_per_second, cfg.rate_burst);

    let v1 = Router::new()
        .route(
            "/ingest",
            post(routes::ingest::ingest_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("ingest"),
                )),
        )
        .route(
            "/recall",
            post(routes::recall::recall_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("recall"),
                )),
        )
        .route(
            "/forget",
            post(routes::forget::forget_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("forget"),
                )),
        )
        .route(
            "/snapshot/{lsn}",
            get(routes::snapshot::snapshot_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("recall"),
                )),
        );

    Router::new()
        .nest("/v1", v1)
        .route("/healthz", get(routes::healthz::healthz_handler))
        .layer(middleware::cors::cors_layer(&cfg.cors_origins))
        .with_state(state)
}

/// Build a per-route auth closure that injects `RequiredScope(scope)` into the
/// request extensions BEFORE delegating to `auth_middleware`. This guarantees
/// the scope check fires on every authenticated request (the alternative —
/// composing two `from_fn` middleware in the right order — is brittle because
/// `route_layer` ordering wraps innermost-first, which conflicts with the
/// "enrich → auth → handler" semantics we want).
fn scoped_auth(
    scope: &'static str,
) -> impl Clone
+ Send
+ Sync
+ 'static
+ Fn(
    axum::extract::State<AppState>,
    axum::extract::Request,
    axum::middleware::Next,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = axum::response::Response> + Send + 'static>,
> {
    move |state, mut req, next| {
        req.extensions_mut().insert(RequiredScope(scope));
        Box::pin(auth_middleware(state, req, next))
    }
}

/// Load the bearer-token map from disk (D-07 tokens-file shape).
///
/// On any IO / parse error this returns an empty map — every request will then
/// fail auth with `401`. The boot path logs a warning so operators see the
/// problem; the binary does NOT refuse to start (so a freshly-bootstrapped
/// deployment with no tokens-file can still answer `/healthz`).
fn load_tokens(path: &std::path::Path) -> std::io::Result<crate::state::TokenMap> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
