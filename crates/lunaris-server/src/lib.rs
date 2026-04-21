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
pub fn build(cfg: Config, lunaris: Arc<lunaris::Lunaris>) -> Router {
    let tokens = load_tokens(&cfg.tokens_file).unwrap_or_default();
    let state = AppState::new(lunaris, tokens);

    // Per-route scope enrichers — attach `RequiredScope` to the request
    // extensions so `auth_middleware` rejects tokens lacking the scope. The
    // closures live behind `from_fn` to keep the route declaration readable.
    async fn enrich_ingest(
        mut req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        req.extensions_mut().insert(RequiredScope("ingest"));
        next.run(req).await
    }
    async fn enrich_recall(
        mut req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        req.extensions_mut().insert(RequiredScope("recall"));
        next.run(req).await
    }
    async fn enrich_forget(
        mut req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        req.extensions_mut().insert(RequiredScope("forget"));
        next.run(req).await
    }
    async fn enrich_snapshot(
        mut req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        req.extensions_mut().insert(RequiredScope("recall"));
        next.run(req).await
    }

    let v1 = Router::new()
        .route(
            "/ingest",
            post(routes::ingest::ingest_handler)
                .route_layer(axum::middleware::from_fn(enrich_ingest)),
        )
        .route(
            "/recall",
            post(routes::recall::recall_handler)
                .route_layer(axum::middleware::from_fn(enrich_recall)),
        )
        .route(
            "/forget",
            post(routes::forget::forget_handler)
                .route_layer(axum::middleware::from_fn(enrich_forget)),
        )
        .route(
            "/snapshot/{lsn}",
            get(routes::snapshot::snapshot_handler)
                .route_layer(axum::middleware::from_fn(enrich_snapshot)),
        )
        .layer(middleware::rate_limit::governor_layer(
            cfg.rate_per_second,
            cfg.rate_burst,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1)
        .route("/healthz", get(routes::healthz::healthz_handler))
        .layer(middleware::cors::cors_layer(&cfg.cors_origins))
        .with_state(state)
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
