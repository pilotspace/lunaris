//! lunaris-ingest — markdown chunker + ingest pipeline (chunk → embed → `atomic_write`).
//!
//! Closes INGEST-01..04 of Phase 2 hot path:
//! - **INGEST-01**: markdown-aware chunker, 500 token target, 100 overlap, heading
//!   path preserved on every chunk.
//! - **INGEST-02**: batched embedder driver — 32 chunks per `embed_batch` call
//!   with per-chunk fallback on batch failure.
//! - **INGEST-03**: public `ingest_episode` function — wrapped by `Lunaris::ingest`
//!   in Plan 02-01 Task 3.
//! - **INGEST-04**: single `StoragePort::atomic_write` call per Episode (Episode +
//!   all Chunks fan into one all-or-nothing batch).
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod chunker;
pub mod pipeline;

pub use chunker::{ChunkDraft, chunk_markdown, est_token_count};
pub use pipeline::{INGEST_EMBED_BATCH_SIZE, ingest_episode};

// Re-export the keyspace helpers from the moon backend so callers don't depend
// directly on a backend crate (Phase 1 convention — Moon's keyspace.rs is the
// canonical key format; Postgres `lunaris_kv` uses the same byte prefix
// convention).
pub use lunaris_storage_moon::keyspace::{chunk_key, chunk_prefix, episode_key, episode_prefix};
