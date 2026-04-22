//! Composable Lunaris recipe primitives.
//!
//! Each primitive wraps an [`std::sync::Arc<lunaris::Lunaris>`] handle and
//! exposes a ≤ 30 LOC public surface. Parity tests under `tests/` (Wave 3)
//! enforce Moon + Postgres semantic equivalence per Phase 4 Plan 04-03
//! `probe_backend` discipline.
//!
//! ## Primitives
//!
//! - [`MessageStream`] — recency-weighted message recall (ACT-R base-level,
//!   Anderson 1996 `d = 0.5`).
//! - [`DocumentCorpus`] — RRF-fused Vector + Keyword RAG.     *(Wave 2 stub)*
//! - [`TemporalQuery`] — typestate time-travel combinator.    *(Wave 2 stub)*
//! - [`WorkingMemory`] — scope-prefixed scratchpad + Consolidator promotion.
//!   *(Wave 2 stub)*
//!
//! ## Wave 1 scope (Plan 09-01)
//!
//! This wave lands [`MessageStream`] fully implemented and the other three
//! primitives as compiling stubs. Wave 2 Plans 09-02 (DocumentCorpus), 09-03
//! (WorkingMemory), and 09-04 (TemporalQuery) each replace ONE stub file
//! wholesale — `files_modified` sets stay disjoint so the three plans run in
//! parallel without touching this `lib.rs` again.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

mod document_corpus;
mod message_stream;
mod temporal_query;
mod working_memory;

pub mod conversational;
pub mod documentary;

pub use document_corpus::DocumentCorpus;
pub use message_stream::MessageStream;
pub use temporal_query::{Documents, Facts, Messages, SupportsAsOf, SupportsBetween, TemporalQuery};
pub use working_memory::WorkingMemory;
