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
// ---------------------------------------------------------------------------
// W4.18 second half — `docs/` pages OUTSIDE `docs/book/src/`.
//
// The book half of W4.18 covered `docs/book/src/**` only. 116 further Rust
// fences live in `docs/` proper and no tool touched any of them, which is how
// 28 of the 45 stale call sites W4.17 repaired by hand got there.
//
// NOT every `docs/` page belongs here, and the exclusions are a decision, not
// a backlog. Three families are deliberately out, each for a reason that does
// not expire:
//
//  * `docs/rfcs/**`, `docs/design/**`, `docs/decisions/**`, `docs/spikes/**` —
//    records of what was decided AT A POINT IN TIME. Compiling them against
//    today's API would force edits that falsify the record.
//  * `docs/migration/**` — these SHOW the old API next to the new one. A
//    migration guide whose "before" block compiles is a migration guide that
//    has been rewritten into uselessness.
//  * `docs/integration/helios-memory-engine.md` — its Rust is illustrative
//    code for `helios_memory`, a crate in the DOWNSTREAM repo (`grep helios`
//    over this workspace returns nothing). Compiling it would mean stubbing
//    that crate here, which would assert something about the stub and nothing
//    about Lunaris. Its §3 additionally mirrors the deleted candle
//    constructors on purpose, under an in-page correction banner.
//
// `scripts/tests/test_book_fences_are_compiled.py` pins this list so a page
// cannot drop out of coverage quietly.
#[doc = include_str!("../../../docs/guide.md")]
pub mod docs_guide {}
#[doc = include_str!("../../../docs/helios-integration.md")]
pub mod docs_helios_integration {}
#[doc = include_str!("../../../docs/MIGRATING-FROM-ZEP.md")]
pub mod docs_migrating_from_zep {}
#[doc = include_str!("../../../docs/operations/external-moon.md")]
pub mod docs_operations_external_moon {}
#[doc = include_str!("../../../docs/protocol/conformance.md")]
pub mod docs_protocol_conformance {}

// Its one Rust fence is a tombstone crate's `lib.rs` and names no Lunaris
// symbol, so compiling it detects no drift. It is here anyway: inclusion is
// what subjects the page to the untagged-fence guard, and that guard is what
// found the two shell transcripts on this page that rustdoc was about to
// compile as Rust.
#[doc = include_str!("../../../docs/release/deprecations.md")]
pub mod docs_release_deprecations {}
