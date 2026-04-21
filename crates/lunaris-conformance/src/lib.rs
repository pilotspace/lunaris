//! Plan 05-02 + 05-03 — `lunaris-conformance` harness.
//!
//! Two re-usable suites that any backend / server impl can certify against:
//!
//! - [`storage`] — parameterized over `Arc<dyn StoragePort>`. Run via
//!   `run_full_storage_suite(storage).await`. Tests every method on the
//!   `StoragePort` trait surface from Phase 1 STORE-01 (D-17).
//! - [`protocol`] — parameterized over `(reqwest::Client, base_url, token)`.
//!   Run via `run_full_protocol_suite(client, base_url, token).await`. Tests
//!   the four MemoryProtocol verbs + SSE + auth + rate limit + retrieval
//!   modes (D-11). **Body lands in Plan 05-03; this plan ships the empty
//!   stub so `lib.rs` re-export compiles and Plan 05-01 / 05-02 / 05-04
//!   builds don't block on Plan 05-03.**
//! - [`fixtures`] — small fixture corpus (10 episodes) + query set + helper
//!   seeders (`seed_three_chunks` + `seed_one_edge`) used by the storage
//!   suite + AS_OF parity test (STORE-07).
//!
//! The Plan 04-03 chaos test (`tests/crash_recovery.rs`) is preserved
//! verbatim — Plan 05-02 only ADDS to this crate, never modifies that file.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod fixtures;
pub mod protocol;
pub mod storage;

pub use protocol::run_full_protocol_suite;
pub use storage::run_full_storage_suite;
