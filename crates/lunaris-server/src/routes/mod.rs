//! Plan 05-01 routes — module declarations.
//!
//! - [`healthz`] — open probe surface (no auth).
//! - [`ingest`] — `POST /v1/ingest` (PROTO-01).
//! - [`recall`] — `POST /v1/recall` with SSE branch (PROTO-02 + PROTO-03).
//! - [`forget`] — `POST /v1/forget` (Plan 04-05 surface + D-21 two-step rail).
//! - [`snapshot`] — `GET /v1/snapshot/{lsn}` NDJSON stream.

pub mod forget;
pub mod healthz;
pub mod ingest;
pub mod recall;
pub mod snapshot;
