//! HTTP + SSE memory server for [Lunaris].
//!
//! `lunaris-server` is an [axum]-based service that exposes a
//! [`lunaris::Lunaris`] handle over **MemoryProtocol 0.1** (see
//! `docs/protocol/memoryprotocol-0.1.md`). It is the network front door for
//! agent platforms that talk to a shared Lunaris deployment rather than
//! linking the engine directly.
//!
//! ## Routes
//!
//! | Method & path             | Purpose                                  |
//! |---------------------------|------------------------------------------|
//! | `POST /v1/ingest`         | ingest an episode; returns the assigned `Lsn` |
//! | `POST /v1/recall`         | run a retrieval-DSL query; streams hits as Server-Sent Events (`event: hit`) |
//! | `POST /v1/forget`         | forget by scope / target                  |
//! | `GET  /v1/snapshot/{lsn}` | NDJSON stream of every primitive visible at `{lsn}`; `404` if `{lsn}` wall_ms is strictly in the future |
//! | `GET  /v1/episode/{id}`   | fetch a single episode by ULID; `400` on bad ULID, `404` if absent in scope |
//! | `GET  /healthz`           | unauthenticated liveness probe            |
//! | `GET  /metrics`           | Prometheus text-format metrics (root, no Bearer) |
//!
//! ## Security
//!
//! Every `/v1/*` route requires a `Bearer` token (mapped to a tenant scope via
//! the tokens file) and is subject to a per-tenant token-bucket rate limit. The
//! JWT/tenant claim is the only source of truth for the partition scope —
//! wire-side `scope` fields are ignored. An outer CORS layer is applied per the
//! configured origin list.
//!
//! ## Configuration
//!
//! The server is configured entirely via CLI flags / matching environment
//! variables — see [`config::Config`] and the Lunaris book's **Operations**
//! chapter for the full table.
//!
//! ## Module map
//!
//! - [`config`] — clap `Config` struct (D-01..D-09 flags + matching env vars).
//! - [`state`] — `AppState { lunaris: Arc<Lunaris>, tokens, runtime_flags }`.
//! - [`dto`] — JSON wire DTOs (`IngestBody`, `IngestResponse`, `RecallRequest`, `RetrievalMode`, `ForgetRequestDto`).
//! - [`shutdown`] — `tokio::sync::Notify` graceful-shutdown wrapper.
//! - [`routes`] — per-verb handler modules (`ingest`, `recall`, `forget`, `snapshot`, `episode`, `healthz`).
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
//!
//! [Lunaris]: https://github.com/lunaris-dev/lunaris
//! [axum]: https://docs.rs/axum

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

pub mod config;
pub mod dto;
// hotkeys-observability — 10s HOTKEYS poller feeding lunaris_hotkey_samples.
pub mod hotkeys_poller;
// Plan 05-05 OPS-06 — Prometheus metrics registry + GET /metrics text-format
// handler + 10s queue-depth poller (background tokio task).
pub mod metrics;
pub mod middleware;
pub mod queue_depth_poller;
pub mod routes;
pub mod shutdown;
pub mod state;

pub use config::{Command, Config, OpsCli};
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

    // Plan 05-05 OPS-06 — propagate the `--metrics-disabled` flag into the
    // runtime-flags toggle so `routes::metrics::metrics_handler` returns 404
    // when the flag is set. Three-surface toggle convention (PATTERNS.md
    // Shared Pattern 4) — operators may also flip this at runtime later via
    // a future `/admin/flags` control endpoint without restart.
    if cfg.metrics_disabled {
        *state.runtime_flags.metrics_disabled.write() = true;
    }

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

    // Plan 05-05 OPS-05 — per-route stacking now includes the tracing
    // middleware that wraps the request in `lunaris.server.handle_request`
    // info_span. Stacking order on the request path:
    //
    //   request → scoped_auth → tracing → rate_limit → handler
    //
    // Why: `tracing_middleware` reads `AuthClaims.tenant` from request
    // extensions for the `tenant` span field, so it MUST run AFTER
    // `auth_middleware`. `route_layer` adds OUTER on each subsequent call,
    // so we add rate_limit first (innermost), then tracing, then
    // scoped_auth (outermost = runs first on request). This guarantees:
    //   1. scoped_auth populates AuthClaims before tracing reads it.
    //   2. tracing's `lunaris.server.handle_request` span wraps the
    //      downstream `lunaris.{ingest,recall,forget}` spans as a parent.
    //   3. rate_limit's TenantKey extractor still sees AuthClaims (auth
    //      ran first).
    let v1 = Router::new()
        .route(
            "/ingest",
            post(routes::ingest::ingest_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn(middleware::tracing::tracing_middleware))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("ingest"),
                )),
        )
        .route(
            "/recall",
            post(routes::recall::recall_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn(middleware::tracing::tracing_middleware))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("recall"),
                )),
        )
        .route(
            "/forget",
            post(routes::forget::forget_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn(middleware::tracing::tracing_middleware))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("forget"),
                )),
        )
        .route(
            "/snapshot/{lsn}",
            get(routes::snapshot::snapshot_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn(middleware::tracing::tracing_middleware))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("recall"),
                )),
        )
        .route(
            "/episode/{id}",
            get(routes::episode::episode_handler)
                .route_layer(rate_limit.clone())
                .route_layer(axum::middleware::from_fn(middleware::tracing::tracing_middleware))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    scoped_auth("recall"),
                )),
        );

    Router::new()
        .nest("/v1", v1)
        .route("/healthz", get(routes::healthz::healthz_handler))
        // Plan 05-05 OPS-06 — `/metrics` mounted at root (NOT under `/v1`)
        // so Prometheus scrapers reach it without Bearer-auth tokens.
        // CONTEXT.md D-25 + threat-model T-05-05-05 (operators MUST front
        // with network ACL or reverse-proxy auth in production — documented
        // in spec markdown, see Plan 05-05 Task 3).
        .route("/metrics", get(routes::metrics::metrics_handler))
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
