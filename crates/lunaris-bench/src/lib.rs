//! lunaris-bench — Phase 2 latency budget enforcement.
//!
//! This crate is bench-only. It exposes:
//!
//! - [`fixtures`] — the 12 KB markdown fixture from `lunaris-ingest` re-exported
//!   so Criterion benches don't take a path dep on test-only `tests/fixtures/`.
//! - [`corpus`] — the deterministic 1M-fact synthetic corpus generator + the
//!   on-disk fingerprint cache so re-runs of the recall bench skip rebuild.
//! - `benches/*.rs` (in this crate) — three Criterion benches that exercise
//!   the Phase 2 hot path: `ingest_hot_path`, `recall_hot_path`,
//!   `atomic_write_hot_path`. Each runs against MoonStorage AND
//!   PostgresStorage when the live backend is reachable; skips gracefully
//!   when not.
//! - `tests/budget_assertions.rs` (gated by feature `budget-it`) — parses
//!   Criterion's `target/criterion/<group>/<bench>/new/{estimates,sample}.json`
//!   and panics with an actionable message when the measured p50/p99 exceeds
//!   the blueprint §4.1/§4.2 budget.
//!
//! NO production code lives here — this is a verification harness. Run benches
//! with `cargo bench -p lunaris-bench` or via `cargo xtask bench` for the
//! cross-backend delta report.
//!
//! ## Determinism contract
//!
//! Both the corpus generator AND the synthetic-query-set helper are seeded by
//! the caller. Default seed is 42 — a separate `seed = 99` is used for the
//! query set so query strings overlap with the corpus vocabulary but aren't
//! literally a corpus fact text (which would let the bench cheat with an exact
//! id-lookup short-circuit).
//!
//! ## Live-backend skip discipline
//!
//! Every bench function probes `MOON_URL` / `PG_URL`. When the env var is unset
//! OR the host:port doesn't accept a 1-second TCP connection, the bench group
//! emits `eprintln!("SKIP {label} bench: {reason}")` and returns without
//! registering a Criterion bench. The budget-assertion test treats missing
//! Criterion JSON as "not enough data" (skip with diagnostic) rather than a
//! false-pass — only present-and-over-budget data fails the gate.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod corpus;
pub mod fixtures;

pub use corpus::{
    BenchCorpusError, CORPUS_BATCH_OPS, CORPUS_EMBEDDING_DIM, CorpusFingerprint, CorpusGenOpts,
    DEFAULT_CORPUS_COUNT, DEFAULT_CORPUS_SEED, DEFAULT_QUERY_SEED, GraphDensity,
    build_chaos_corpus, build_corpus_with_options, build_md_doc_corpus,
    build_one_million_fact_corpus, ensure_corpus_or_skip, fingerprint_path, fingerprint_path_for,
    hash_url, synthetic_query_set, tcp_reachable,
};
pub use fixtures::twelve_kb_markdown_fixture;
