//! Plan 02-04 Task 1 — bench scaffold (real harness lands in Task 2).
//!
//! This empty harness keeps `cargo check -p lunaris-bench --benches` green
//! while Task 1 wires the corpus generator + fixtures. Task 2 replaces this
//! with the real Criterion ingest hot-path bench against MoonStorage AND
//! PostgresStorage.
fn main() {}
