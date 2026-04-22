//! Documentary recipe wrappers (Phase 11 Plan 11-01).
//!
//! Thin compositions of Phase 9/9.1 primitives (`DocumentCorpus`,
//! `TemporalQuery<Documents>`, `MessageStream`) surfacing 5 named recipes
//! from blueprint §7: [`DocumentKnowledgeBase`], [`ResearchPaperCorpus`],
//! [`CodeRepoMemory`], [`TimelineReconstruction`], [`CustomerSupportHistory`].
//!
//! Each wrapper:
//! - has ≤ 6 public `fn` / `async fn` declarations (compile-time test per file)
//! - forwards into at most 2 primitive method invocations per method
//! - holds NO business logic — shape-only composition
//!
//! Phase 11-02 reflects these types through `lunaris-codegen` into the Py +
//! TS bindings; Phase 11-03 closes cross-language parity.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

pub mod code_repo_memory;
pub mod customer_support_history;
pub mod document_knowledge_base;
pub mod research_paper_corpus;
pub mod timeline_reconstruction;

pub use code_repo_memory::CodeRepoMemory;
pub use customer_support_history::CustomerSupportHistory;
pub use document_knowledge_base::DocumentKnowledgeBase;
pub use research_paper_corpus::ResearchPaperCorpus;
pub use timeline_reconstruction::TimelineReconstruction;
