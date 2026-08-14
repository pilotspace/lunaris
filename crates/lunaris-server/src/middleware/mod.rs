//! Plan 05-01 middleware — module declarations.
//!
//! - [`auth`] — Bearer-token validation + scope check (PROTO-05 + D-07).
//! - [`rate_limit`] — tower-governor per-tenant key extractor (PROTO-05 + D-08).
//! - [`cors`] — `tower-http::cors::CorsLayer` factory (D-09).
//! - [`error`] — `LunarisError → HTTP status` mapping (analog
//!   `crates/lunaris/src/forget.rs:30-32` typed-variant pattern).
//! - [`tracing`] — Plan 05-05 OPS-05 per-request `lunaris.server.handle_request`
//!   span + `x-correlation-id` propagation (CONTEXT.md D-24).
//! - [`resilience`] — 0.6.2 P0-1/P0-2 in-flight accounting, request timeout,
//!   concurrency limit + load shedding.

pub mod auth;
pub mod cors;
pub mod error;
pub mod rate_limit;
pub mod resilience;
pub mod tracing;
