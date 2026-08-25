//! SPIKE (W4.18): compile every book rust fence through cargo's doctest path.
//!
//! `mdbook test` CANNOT do this: it passes only `-L/--library-path`, and
//! Rust 2018+ needs `--extern` to bind a crate name, so every fence fails
//! with E0432 `unresolved import lunaris` regardless of its content.
//!
//! Book prose is markdown written for a reader, not rustdoc: it carries bare
//! link lines, lazy list continuations and headings that rustdoc's doc lints
//! read as malformed doc comments. Those lints are about the QUALITY OF A DOC
//! COMMENT, and these are not doc comments — the only thing this crate asserts
//! about the pages is that their Rust compiles.
#![allow(clippy::doc_lazy_continuation, clippy::doc_markdown, clippy::tabs_in_doc_comments)]
#![allow(rustdoc::bare_urls, rustdoc::broken_intra_doc_links, rustdoc::invalid_html_tags)]
#[doc = include_str!("../../../docs/book/src/cookbook/chat-agent.md")]
pub mod cookbook_chat_agent {}
#[doc = include_str!("../../../docs/book/src/cookbook/conversational-channels.md")]
pub mod cookbook_conversational_channels {}
#[doc = include_str!("../../../docs/book/src/cookbook/document-kb.md")]
pub mod cookbook_document_kb {}
#[doc = include_str!("../../../docs/book/src/cookbook/helios-scratchpad.md")]
pub mod cookbook_helios_scratchpad {}
#[doc = include_str!("../../../docs/book/src/cookbook/multi-turn.md")]
pub mod cookbook_multi_turn {}
#[doc = include_str!("../../../docs/book/src/cookbook/querying-three-ways.md")]
pub mod cookbook_querying_three_ways {}
#[doc = include_str!("../../../docs/book/src/cookbook/research-and-code.md")]
pub mod cookbook_research_and_code {}
#[doc = include_str!("../../../docs/book/src/cookbook/support-history.md")]
pub mod cookbook_support_history {}
#[doc = include_str!("../../../docs/book/src/cookbook/timeline.md")]
pub mod cookbook_timeline {}
#[doc = include_str!("../../../docs/book/src/getting-started/architecture.md")]
pub mod getting_started_architecture {}
#[doc = include_str!("../../../docs/book/src/getting-started/concepts.md")]
pub mod getting_started_concepts {}
#[doc = include_str!("../../../docs/book/src/getting-started/installation.md")]
pub mod getting_started_installation {}
#[doc = include_str!("../../../docs/book/src/getting-started/quickstart.md")]
pub mod getting_started_quickstart {}
#[doc = include_str!("../../../docs/book/src/guides/consolidate-verify.md")]
pub mod guides_consolidate_verify {}
#[doc = include_str!("../../../docs/book/src/guides/forget.md")]
pub mod guides_forget {}
#[doc = include_str!("../../../docs/book/src/guides/graph.md")]
pub mod guides_graph {}
#[doc = include_str!("../../../docs/book/src/guides/ingest.md")]
pub mod guides_ingest {}
#[doc = include_str!("../../../docs/book/src/guides/multi-agent.md")]
pub mod guides_multi_agent {}
#[doc = include_str!("../../../docs/book/src/guides/retrieval-dsl.md")]
pub mod guides_retrieval_dsl {}
#[doc = include_str!("../../../docs/book/src/introduction.md")]
pub mod introduction {}
#[doc = include_str!("../../../docs/book/src/protocol/conformance.md")]
pub mod protocol_conformance {}
#[doc = include_str!("../../../docs/book/src/reference/api.md")]
pub mod reference_api {}
