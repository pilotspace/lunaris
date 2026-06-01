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
    BakoffConfig, BpeTokenCounter, CandidateGenerator, ChunkDraft, ChunkSelector, DocTree,
    EntityRef, GeneratorContext, HeadingRecord, MetricContext, RecursiveSplitMergeGenerator,
    ScoredCandidate, SegmentMode, SelectorWeights, SemanticBreakpointGenerator,
    SizeVariantGenerator, StructuralGenerator, SurrogateTokenCounter, TextUnit, TocNode, TocNodeId,
    TokenCounter, UnitKind, build_doctree, build_raptor_tree, chunk_markdown,
    chunk_markdown_with_counter, chunk_markdown_with_headings,
    chunk_markdown_with_headings_with_counter, chunk_texts_for_community, est_token_count,
    make_token_counter, raptor_community_id, run_bakeoff, segment_units,
};
pub use pipeline::{
    INGEST_EMBED_BATCH_SIZE, ingest_episode, ingest_episode_with_bakeoff,
    ingest_episode_with_counter,
};
pub use schema_gate::{SchemaError, validate_chunk_metadata, validate_chunk_text};

// Wave 2.5B: re-export the primitive KV key helpers from lunaris-core (moved
// from lunaris-storage-moon so the engine layer has no infra dependency for keys).
pub use lunaris_core::keyspace::{
    chunk_key, chunk_prefix, doctree_key, doctree_prefix, episode_key, episode_prefix,
};
