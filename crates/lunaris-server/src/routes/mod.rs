//! Plan 05-01 routes — module declarations.
//!
//! - [`healthz`] — open probe surface (no auth).
//! - [`ingest`] — `POST /v1/ingest` (PROTO-01).
//! - [`recall`] — `POST /v1/recall` with SSE branch (PROTO-02 + PROTO-03).
//! - [`forget`] — `POST /v1/forget` (Plan 04-05 surface + D-21 two-step rail).
//! - [`snapshot`] — `GET /v1/snapshot/{lsn}` NDJSON stream.
//! - [`episode`] — `GET /v1/episode/{id}` single-episode lookup by ULID.
//! - [`browse`] — `GET /v1/scopes` + `GET /v1/browse/{kind}` (Memory Inspector
//!   Phase 1: paginated, JWT-scoped read surface over `scan_page`).
//! - [`detail`] — `GET /v1/detail/{kind}/{id}` (Memory Inspector Phase 1:
//!   single-primitive detail with provenance resolved to source episodes).
//! - [`metrics`] — `GET /metrics` Prometheus text format (Plan 05-05 OPS-06;
//!   open probe surface, NOT under `/v1`).

pub mod browse;
pub mod detail;
pub mod episode;
pub mod forget;
pub mod healthz;
pub mod ingest;
pub mod metrics;
pub mod recall;
pub mod snapshot;
