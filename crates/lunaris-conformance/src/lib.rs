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
pub mod raptor;
pub mod skip;
pub mod storage;
pub mod suite_scope;

// Phase 12 Plan 12-04 — chaos / fsck helpers shared with the Helios
// SIGKILL test. Always-on (no feature gate) so downstream integration
// tests in `crates/lunaris` can import without chaining `chaos-it`.
pub mod chaos_helpers;

// Plan 08-04 (revision iteration 2) — per-driver backend parity module.
// Gated behind the `bindings-it` feature so default `cargo test --workspace`
// does not compile the parity-test surface. Consumers:
//   - `tests/run_bindings_backend_parity.rs` — Rust-driver integration test.
//   - `crates/lunaris-py/tests/test_backend_parity.py` — Python-driver test.
//   - `crates/lunaris-ts/__test__/backend_parity.spec.mts` — TS-driver test.
// All three compare their own rows against the committed golden reference
// at `fixtures/golden/bindings_fixture.json` — NOT against each other.
#[cfg(feature = "bindings-it")]
pub mod bindings;

pub use protocol::run_full_protocol_suite;
pub use storage::run_full_storage_suite;
