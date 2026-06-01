//! Markdown-aware chunker — INGEST-01.
//!
//! Submodules:
//! - [`token`]: [`TokenCounter`] trait, [`BpeTokenCounter`], [`SurrogateTokenCounter`],
//!   and [`make_token_counter`] factory (CHUNK-01).
//! - [`segment`]: [`segment_units`] — paragraph + sentence unit segmenter (STRUCT-01).
//! - [`tree`]: [`DocTree`] / [`TocNode`] build and keyspace helpers (STRUCT-02).
//!
//! ## Algorithm
//!
//! 1. Walk a [`pulldown_cmark::Parser`] event stream with all CommonMark
//!    extensions enabled (footnotes, tables, strikethrough, task lists).
//! 2. Maintain a `heading_stack: Vec<(level, text)>` — pushed at `Tag::Heading`
//!    start, populated from `Event::Text` while the heading is the innermost
//!    open block, finalised at `Tag::Heading` end. New headings pop entries
//!    with `level >= incoming_level` before pushing (the standard nesting rule).
//! 3. Accumulate plain text from `Event::Text(s)` and `Event::Code(s)` (inline
//!    code) outside heading blocks into `current_text`. Block code, paragraphs
//!    and list items contribute their visible text the same way.
//! 4. After every text appendix, recompute the token estimate (whitespace
//!    word count × 1.3 — documented as a v0 BPE surrogate). When the estimate
//!    crosses `target_tokens`, emit a [`ChunkDraft`] capturing the accumulated
//!    text + the current heading path snapshot, then reset `current_text` to
//!    the last `overlap_tokens` worth of words (preserving word boundaries).
//! 5. At end-of-stream, flush the remainder.
//!
//! ## Token-count heuristic (v0)
//!
//! `est_token_count(text) = ceil(text.split_whitespace().count() * 1.3)`.
//!
//! This is the v0 BPE surrogate. EmbeddingGemma uses SentencePiece BPE which
//! averages ~1.3 sub-word tokens per whitespace-delimited word for English
//! prose; deeper precision (full BPE round-trip via `tokenizers`) is a v1
//! polish. **Plan 02-04 (latency benches) MUST use the same heuristic for the
//! bench fixture so the chunk count assertions stay stable across the plan.**
//!
//! ## Fixture chunk-count target
//!
//! For a 12 KB markdown doc (~3000 estimated tokens) at `target=500 overlap=100`,
//! expect 4–8 chunks. The fixture in `tests/fixtures/12kb_doc.md` is hand-tuned
//! to land squarely in that range.

pub mod candidate;
pub mod metrics;
pub mod segment;
pub mod selector;
pub mod token;
pub mod tree;

pub use candidate::{
    BakoffConfig, CandidateGenerator, GeneratorContext, RecursiveSplitMergeGenerator,
    ScoredCandidate, SemanticBreakpointGenerator, SizeVariantGenerator, StructuralGenerator,
    run_bakeoff,
};
pub use metrics::{
    BlockIntegrityMetric, CandidateWithEmbeddings, ContextualCoherenceMetric, EntityRef,
    IntrachunkCohesionMetric, MetricContext, ReferenceIntegrityMetric, SizeComplianceMetric,
};
pub use segment::{SegmentMode, TextUnit, UnitKind, segment_units};
pub use selector::{ChunkSelector, SelectorWeights};
pub use token::{BpeTokenCounter, SurrogateTokenCounter, TokenCounter, make_token_counter};
pub use tree::{DocTree, HeadingRecord, TocNode, TocNodeId, build_doctree};

use lunaris_core::{Chunk, HlcClock, Scope};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ulid::Ulid;

// Lazy static surrogate used by the default pipeline paths.
static SURROGATE: SurrogateTokenCounter = SurrogateTokenCounter;

/// One pre-embedding chunk produced by [`chunk_markdown`].
///
/// Convert to a Phase 1 [`Chunk`] via [`ChunkDraft::into_chunk`] (which assigns
/// the chunk a fresh [`Ulid`] and stamps it with `BiTemporal::now(clock)`).
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkDraft {
    pub text: String,
    pub heading_path: Vec<String>,
    /// Sequential chunk index within the source Episode (0-based).
    pub offset: u32,
    pub tokens: u32,
    /// The trailing `overlap_tokens` words of `text` — kept on the draft so the
    /// retriever can reconstruct the overlap region when displaying highlights.
    pub overlap_tail: String,
}

impl ChunkDraft {
    /// Convert to a Phase 1 [`Chunk`]. The caller controls the `episode_id`;
    /// `clock` issues the `BiTemporal::now` stamp.
    ///
    /// Build a Phase 1 [`Chunk`] under `scope`. The caller MUST forward the
    /// owning Episode's [`Scope`] so the chunk inherits the same partition
    /// key (the typical pattern is `draft.into_chunk(episode.scope.clone(),
    /// episode.id, &clock)`).
    pub fn into_chunk(self, scope: Scope, episode_id: Ulid, clock: &HlcClock) -> Chunk {
        let mut c = Chunk::new(
            scope,
            episode_id,
            self.text,
            self.tokens,
            self.offset,
            self.heading_path,
            clock,
        );
        c.overlap_tail = self.overlap_tail;
        c
    }
}

/// v0 BPE surrogate: `ceil(words * 1.3)`. See module-level rationale.
///
/// Retained as a free function for callers that do not need the full
/// [`TokenCounter`] trait dispatch (e.g. bench fixtures, the `segment` module).
#[inline]
pub fn est_token_count(s: &str) -> u32 {
    SurrogateTokenCounter.count(s)
}

/// Produce the trailing `n` whitespace-delimited words of `s`, joined by single
/// spaces. Preserves word boundaries (no mid-word splits).
fn tail_n_tokens(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let take = words.len().min(n);
    let start = words.len() - take;
    words[start..].join(" ")
}

/// Chunk a markdown document into fixed-token-target chunks with heading-path
/// metadata and a sliding overlap window.
///
/// # Parameters
/// - `text`: raw markdown source
/// - `target_tokens`: emit a chunk when accumulated text crosses this estimate
/// - `overlap_tokens`: number of trailing words carried over to the next chunk
///
/// # Output
/// Vec of [`ChunkDraft`] in source order. `offset` is monotonic from 0.
///
/// # Edge cases
/// - Empty input → `Vec::new()` (no zero-text chunks).
/// - All-whitespace input → `Vec::new()`.
/// - Single huge code block exceeding `target_tokens` → emitted as one
///   oversized chunk (we do NOT split mid-code-block in v0; preserves
///   semantic atomicity for code retrieval).
/// - `overlap_tokens >= target_tokens` → caller error in spirit, but we
///   handle it by emitting normally then carrying the entire chunk text as
///   the overlap (no infinite loop guarantee: the post-emit reset still
///   shrinks the working text monotonically per chunk).
pub fn chunk_markdown(text: &str, target_tokens: usize, overlap_tokens: usize) -> Vec<ChunkDraft> {
    chunk_markdown_with_counter(text, target_tokens, overlap_tokens, &SURROGATE)
}

/// Chunk a markdown document using a caller-supplied [`TokenCounter`].
///
/// This is the canonical "new path" wired to a real BPE tokenizer when
/// `make_token_counter(Some(path))` returns a [`BpeTokenCounter`]. The default
/// pipeline convenience functions ([`chunk_markdown`], [`chunk_markdown_with_headings`])
/// delegate here with [`SurrogateTokenCounter`] so their output is byte-identical
/// to the v0 baseline (preserving INGEST-04 / bench fixture stability).
///
/// # Parameters
/// - `text`: raw markdown source
/// - `target_tokens`: emit a chunk when accumulated token count crosses this threshold
/// - `overlap_tokens`: number of trailing words carried over to the next chunk
/// - `counter`: any [`TokenCounter`] impl — e.g. `&SurrogateTokenCounter` or
///   `&BpeTokenCounter`
pub fn chunk_markdown_with_counter(
    text: &str,
    target_tokens: usize,
    overlap_tokens: usize,
    counter: &dyn TokenCounter,
) -> Vec<ChunkDraft> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let parser = Parser::new_ext(text, Options::all());
    let mut state = ChunkerState::new(target_tokens, overlap_tokens, counter);

    for event in parser {
        state.process(event);
    }
    state.flush_final();
    state.into_chunks()
}

/// Chunk a markdown document and simultaneously extract [`HeadingRecord`]s for
/// [`DocTree`] construction.
///
/// Uses `pulldown_cmark::Parser::into_offset_iter()` so heading byte spans
/// are available; converts to character offsets for [`HeadingRecord::char_span`].
///
/// Returns `(Vec<ChunkDraft>, Vec<HeadingRecord>)`. The chunks are identical to
/// those produced by `chunk_markdown`; the heading records are used by the
/// ingest pipeline to build and persist the [`DocTree`].
pub fn chunk_markdown_with_headings(
    text: &str,
    target_tokens: usize,
    overlap_tokens: usize,
) -> (Vec<ChunkDraft>, Vec<HeadingRecord>) {
    if text.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }

    let parser = Parser::new_ext(text, Options::all()).into_offset_iter();
    let mut state = ChunkerState::new(target_tokens, overlap_tokens, &SURROGATE);
    // Track open heading byte-range start for span capture.
    let mut heading_byte_start: Option<usize> = None;

    for (event, byte_range) in parser {
        match &event {
            Event::Start(Tag::Heading { .. }) => {
                heading_byte_start = Some(byte_range.start);
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(start_byte) = heading_byte_start.take() {
                    // Convert byte offsets to char offsets.
                    let start_char = text[..start_byte].chars().count();
                    let end_char = text[..byte_range.end].chars().count();
                    // The last entry pushed to heading_stack is the one we just closed.
                    if let Some((lvl, title)) = state.heading_stack.last() {
                        state.heading_records.push(HeadingRecord {
                            level: *lvl,
                            title: title.clone(),
                            char_span: (start_char, end_char),
                        });
                    }
                }
            }
            _ => {}
        }
        state.process(event);
    }
    state.flush_final();
    let records = state.heading_records.clone();
    (state.into_chunks(), records)
}

/// Chunk a markdown document using a caller-supplied [`TokenCounter`] and
/// simultaneously extract [`HeadingRecord`]s for [`DocTree`] construction.
///
/// This is the canonical production path used by `ingest_episode_with_counter`:
/// it uses a real BPE counter when one is available (via `make_token_counter`),
/// falling back to [`SurrogateTokenCounter`] when no tokenizer file is
/// configured.
///
/// Uses `pulldown_cmark::Parser::into_offset_iter()` so heading byte spans
/// are available; converts to character offsets for [`HeadingRecord::char_span`].
///
/// Returns `(Vec<ChunkDraft>, Vec<HeadingRecord>)`.
pub fn chunk_markdown_with_headings_with_counter(
    text: &str,
    target_tokens: usize,
    overlap_tokens: usize,
    counter: &dyn TokenCounter,
) -> (Vec<ChunkDraft>, Vec<HeadingRecord>) {
    if text.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }

    let parser = Parser::new_ext(text, Options::all()).into_offset_iter();
    let mut state = ChunkerState::new(target_tokens, overlap_tokens, counter);
    // Track open heading byte-range start for span capture.
    let mut heading_byte_start: Option<usize> = None;

    for (event, byte_range) in parser {
        match &event {
            Event::Start(Tag::Heading { .. }) => {
                heading_byte_start = Some(byte_range.start);
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(start_byte) = heading_byte_start.take() {
                    // Convert byte offsets to char offsets.
                    let start_char = text[..start_byte].chars().count();
                    let end_char = text[..byte_range.end].chars().count();
                    // The last entry pushed to heading_stack is the one we just closed.
                    if let Some((lvl, title)) = state.heading_stack.last() {
                        state.heading_records.push(HeadingRecord {
                            level: *lvl,
                            title: title.clone(),
                            char_span: (start_char, end_char),
                        });
                    }
                }
            }
            _ => {}
        }
        state.process(event);
    }
    state.flush_final();
    let records = state.heading_records.clone();
    (state.into_chunks(), records)
}

/// Internal walker state.
struct ChunkerState<'c> {
    target_tokens: usize,
    overlap_tokens: usize,
    /// Stack of (level, text) for the currently open heading lineage.
    heading_stack: Vec<(u8, String)>,
    /// `Some(level)` while a `Tag::Heading` is open — text events go to
    /// `heading_stack.last_mut()` instead of `current_text`.
    open_heading_level: Option<u8>,
    /// Accumulated visible text for the current chunk.
    current_text: String,
    /// Sequential chunk index.
    offset: u32,
    /// Output buffer.
    chunks: Vec<ChunkDraft>,
    /// Captured heading records for DocTree construction (STRUCT-02).
    /// Populated only when using [`chunk_markdown_with_headings`].
    heading_records: Vec<HeadingRecord>,
    /// Token counter used to decide when to emit a chunk.
    counter: &'c dyn TokenCounter,
}

impl<'c> ChunkerState<'c> {
    fn new(target_tokens: usize, overlap_tokens: usize, counter: &'c dyn TokenCounter) -> Self {
        Self {
            target_tokens,
            overlap_tokens,
            heading_stack: Vec::new(),
            open_heading_level: None,
            current_text: String::new(),
            offset: 0,
            chunks: Vec::new(),
            heading_records: Vec::new(),
            counter,
        }
    }

    fn process(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let lvl = heading_level_to_u8(level);
                // Pop sibling/ancestor headings of equal or deeper level.
                while self.heading_stack.last().map(|(l, _)| *l >= lvl).unwrap_or(false) {
                    self.heading_stack.pop();
                }
                self.heading_stack.push((lvl, String::new()));
                self.open_heading_level = Some(lvl);
            }
            Event::End(TagEnd::Heading(_)) => {
                self.open_heading_level = None;
            }
            Event::Text(s) => {
                if let Some(_lvl) = self.open_heading_level {
                    if let Some((_, txt)) = self.heading_stack.last_mut() {
                        if !txt.is_empty() {
                            txt.push(' ');
                        }
                        txt.push_str(&s);
                    }
                } else {
                    self.append_body_text(&s);
                }
            }
            Event::Code(s) => {
                if self.open_heading_level.is_some() {
                    if let Some((_, txt)) = self.heading_stack.last_mut() {
                        if !txt.is_empty() {
                            txt.push(' ');
                        }
                        txt.push_str(&s);
                    }
                } else {
                    // Inline code follows the same word-level append as text so
                    // a long inline `code` block doesn't blow past the chunk
                    // boundary in a single event.
                    self.append_body_text(&s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.open_heading_level.is_none() && !self.current_text.is_empty() {
                    self.current_text.push(' ');
                }
            }
            Event::End(TagEnd::Paragraph) => {
                // Paragraph break — gentle separator without forcing a chunk emit.
                if self.open_heading_level.is_none() && !self.current_text.ends_with(' ') {
                    self.current_text.push(' ');
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => {
                if !self.current_text.is_empty() && !self.current_text.ends_with(' ') {
                    self.current_text.push(' ');
                }
            }
            _ => {}
        }
    }

    fn append_body_text(&mut self, s: &str) {
        // pulldown-cmark coalesces a paragraph's plain text into one Event::Text,
        // so a single event can carry far more than `target_tokens` worth of words.
        // Walk the incoming text word-by-word so we can emit mid-paragraph chunks.
        for word in s.split_whitespace() {
            if !self.current_text.is_empty() && !self.current_text.ends_with(' ') {
                self.current_text.push(' ');
            }
            self.current_text.push_str(word);
            self.maybe_emit();
        }
    }

    fn maybe_emit(&mut self) {
        if self.counter.count(&self.current_text) as usize >= self.target_tokens {
            self.emit_chunk();
        }
    }

    fn emit_chunk(&mut self) {
        let text = self.current_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let tokens = self.counter.count(&text);
        let overlap_tail = tail_n_tokens(&text, self.overlap_tokens);
        let heading_path: Vec<String> = self
            .heading_stack
            .iter()
            .map(|(_, t)| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        self.chunks.push(ChunkDraft {
            text,
            heading_path,
            offset: self.offset,
            tokens,
            overlap_tail: overlap_tail.clone(),
        });
        self.offset += 1;
        // Carry overlap_tail forward as the seed of the next chunk's text.
        self.current_text = overlap_tail;
        if !self.current_text.is_empty() {
            self.current_text.push(' ');
        }
    }

    fn flush_final(&mut self) {
        let trimmed = self.current_text.trim();
        if trimmed.is_empty() {
            return;
        }
        // Avoid emitting a final chunk that is entirely just overlap_tail from
        // the previous emission (i.e., no new content was added after the last
        // emit). Detect this by comparing against the prior chunk's overlap_tail.
        if let Some(prev) = self.chunks.last()
            && !prev.overlap_tail.is_empty()
            && trimmed == prev.overlap_tail.trim()
        {
            return;
        }
        self.emit_final();
    }

    fn emit_final(&mut self) {
        let text = self.current_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let tokens = self.counter.count(&text);
        let overlap_tail = tail_n_tokens(&text, self.overlap_tokens);
        let heading_path: Vec<String> = self
            .heading_stack
            .iter()
            .map(|(_, t)| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        self.chunks.push(ChunkDraft {
            text,
            heading_path,
            offset: self.offset,
            tokens,
            overlap_tail,
        });
        self.offset += 1;
        self.current_text.clear();
    }

    fn into_chunks(self) -> Vec<ChunkDraft> {
        self.chunks
    }
}

#[inline]
fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_tokenizer_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/test_tokenizer.json")
    }

    #[test]
    fn est_token_count_of_blank_input_is_zero() {
        assert_eq!(est_token_count(""), 0);
        assert_eq!(est_token_count("   \n  "), 0);
    }

    #[test]
    fn est_token_count_applies_v0_heuristic() {
        // 10 words × 1.3 = 13 (ceil)
        let text = "one two three four five six seven eight nine ten";
        assert_eq!(est_token_count(text), 13);
    }

    #[test]
    fn empty_markdown_yields_no_chunks() {
        assert!(chunk_markdown("", 500, 100).is_empty());
        assert!(chunk_markdown("\n\n   \t\n", 500, 100).is_empty());
    }

    #[test]
    fn tail_n_tokens_preserves_word_boundaries() {
        let s = "alpha beta gamma delta";
        assert_eq!(tail_n_tokens(s, 0), "");
        assert_eq!(tail_n_tokens(s, 2), "gamma delta");
        assert_eq!(tail_n_tokens(s, 99), "alpha beta gamma delta");
    }

    /// Prove that `chunk_markdown_with_counter` uses the real BPE counter, not
    /// the surrogate. The committed fixture tokenizer has 0 merges (pure byte
    /// fallback): "hello world" → 11 byte-level tokens vs surrogate's 3.
    ///
    /// This test runs unconditionally via the committed GPT-2-format fixture.
    #[test]
    fn bpe_path_produces_different_token_count_than_surrogate() {
        let path = fixture_tokenizer_path();
        let bpe = BpeTokenCounter::try_new(&path).expect("fixture tokenizer must load");
        let surrogate = SurrogateTokenCounter;

        let text = "hello world";
        let bpe_count = bpe.count(text);
        let surrogate_count = surrogate.count(text);

        // The fixture has 0 merges → byte-level tokens (11 for "hello world").
        // The surrogate gives ceil(2 * 1.3) = 3.
        // These must differ — proving the BPE path is a distinct code path.
        assert_ne!(
            bpe_count, surrogate_count,
            "BPE (fixture, 0 merges) must produce a different count than surrogate \
             (bpe={bpe_count}, surrogate={surrogate_count})"
        );
        // Fixture has 0 merges → one token per byte of "hello world" = 11.
        assert_eq!(bpe_count, 11, "fixture with 0 merges must produce one token per byte");
        // Surrogate: 2 words * 1.3 = 2.6 → ceil = 3.
        assert_eq!(surrogate_count, 3, "surrogate must give ceil(2*1.3)=3 for 'hello world'");

        // Now prove chunk_markdown_with_counter uses the supplied counter, not est_token_count.
        // Use a very low target (5 tokens) so BPE triggers a split on short text
        // while the surrogate (count=3) would NOT trigger a split.
        let md = "hello world foo bar baz";
        let surrogate_chunks = chunk_markdown_with_counter(md, 5, 0, &surrogate);
        let bpe_chunks = chunk_markdown_with_counter(md, 5, 0, &bpe);

        // BPE counts bytes; "hello world foo bar baz" has 23 bytes/chars → many tokens.
        // Surrogate: 5 words * 1.3 = 6.5 → ceil = 7 (> 5 threshold, so 1+ chunks).
        // Both paths must produce at least 1 chunk.
        assert!(!bpe_chunks.is_empty(), "BPE path must produce chunks");
        assert!(!surrogate_chunks.is_empty(), "surrogate path must produce chunks");

        // The BPE chunks must have token counts that match what the BPE counter gives.
        for chunk in &bpe_chunks {
            let expected = bpe.count(&chunk.text);
            assert_eq!(
                chunk.tokens, expected,
                "chunk.tokens must reflect BPE counter (text={:?})",
                chunk.text
            );
        }
        // The surrogate chunks must have token counts matching the surrogate.
        for chunk in &surrogate_chunks {
            let expected = surrogate.count(&chunk.text);
            assert_eq!(
                chunk.tokens, expected,
                "chunk.tokens must reflect surrogate counter (text={:?})",
                chunk.text
            );
        }
    }
}
