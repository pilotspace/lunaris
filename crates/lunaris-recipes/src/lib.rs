//! Composable, opinionated memory recipes for [Lunaris].
//!
//! Where the umbrella `lunaris` crate gives you the raw engine (ingest,
//! retrieval DSL, forget, the optional graph/verify/consolidate pipelines),
//! this crate layers ready-made *recipes* on top — small wrappers (each a
//! ≤ 30 LOC public surface over an `Arc<lunaris::Lunaris>`) that encode the
//! right defaults for a concrete use case so callers don't re-derive them.
//! Every wrapper has Moon + Postgres parity tests under `tests/*_parity.rs`.
//!
//! See the Lunaris book's **Cookbook** chapter for worked examples of each.
//!
//! ## Layering
//!
//! ### 4 primitives — the reusable building blocks
//!
//! - [`MessageStream`] — recency-weighted message recall (ACT-R base-level
//!   activation, Anderson 1996, `d = 0.5`); the substrate for conversational
//!   recipes.
//! - [`DocumentCorpus`] — RRF-fused Vector + Keyword (BM25) retrieval; the
//!   substrate for documentary recipes.
//! - [`TemporalQuery`] — typestate time-travel combinator (`as_of` / `between`
//!   over `Messages` / `Documents` / `Facts` markers).
//! - [`WorkingMemory`] — scope-prefixed scratchpad with `Consolidator`
//!   promotion (re-exported here from [`lunaris::primitives`] so the long-lived
//!   `lunaris_recipes::WorkingMemory` import path stays stable).
//!
//! ### 5 conversational wrappers (`conversational/`)
//!
//! - [`ChatAgentMemory`](conversational::ChatAgentMemory) — a single agent's
//!   running conversation memory.
//! - [`MultiTurnConversation`](conversational::MultiTurnConversation) —
//!   turn-structured dialogue with role attribution.
//! - [`SlackArchive`](conversational::SlackArchive) — channel/thread-aware
//!   chat history.
//! - [`EmailThreading`](conversational::EmailThreading) — reply-chain-aware
//!   mail memory.
//! - [`MeetingNotesMemory`](conversational::MeetingNotesMemory) — transcript +
//!   action-item recall.
//!
//! ### 5 documentary wrappers (`documentary/`)
//!
//! - [`DocumentKnowledgeBase`](documentary::DocumentKnowledgeBase) — general
//!   document KB with hybrid retrieval.
//! - [`ResearchPaperCorpus`](documentary::ResearchPaperCorpus) — paper-aware
//!   corpus (sections, citations).
//! - [`CodeRepoMemory`](documentary::CodeRepoMemory) — source-tree-aware code
//!   memory.
//! - [`TimelineReconstruction`](documentary::TimelineReconstruction) —
//!   bi-temporal event-timeline rebuild.
//! - [`CustomerSupportHistory`](documentary::CustomerSupportHistory) — ticket
//!   / case-history recall.
//!
//! [Lunaris]: https://github.com/lunaris-dev/lunaris

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

mod document_corpus;
mod message_stream;
mod temporal_query;

pub mod conversational;
pub mod documentary;

pub use document_corpus::DocumentCorpus;
pub use message_stream::MessageStream;
pub use temporal_query::{
    Documents, Facts, Messages, SupportsAsOf, SupportsBetween, TemporalQuery,
};
// Phase 12 Option-A relocation — `WorkingMemory` now lives in
// `lunaris::primitives::working_memory` so `HeliosScratchpad` (in the
// `lunaris` crate) can compose over it without a dep cycle. Re-exported
// here so `use lunaris_recipes::WorkingMemory;` keeps compiling for every
// Phase 9/10/11 caller.
pub use lunaris::WorkingMemory;

// W2-L1 — multi-collection weighted-RRF recipe (decision B,
// INTEGRATION-PLAN.md §3.2).
pub use documentary::code_feature_card::{
    CodeFeatureCard, CollectionWeights, HitProvenance, QueryProfile, RecallError, ScoredHit,
};
