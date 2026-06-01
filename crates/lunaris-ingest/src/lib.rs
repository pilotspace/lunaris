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
pub mod schema_gate;

pub use chunker::{
    BpeTokenCounter, ChunkDraft, SegmentMode, SurrogateTokenCounter, TextUnit, TokenCounter,
    UnitKind, chunk_markdown, est_token_count, make_token_counter, segment_units,
};
pub use pipeline::{INGEST_EMBED_BATCH_SIZE, ingest_episode};
pub use schema_gate::{SchemaError, validate_chunk_metadata, validate_chunk_text};

// Wave 2.5B: re-export the primitive KV key helpers from lunaris-core (moved
// from lunaris-storage-moon so the engine layer has no infra dependency for keys).
pub use lunaris_core::keyspace::{chunk_key, chunk_prefix, episode_key, episode_prefix};
