//! Pure-Rust unit segmenter — STRUCT-01.
//!
//! ## Modes
//!
//! - [`SegmentMode::Paragraph`]: walks the pulldown_cmark event stream and
//!   collects text between `Start/End(Paragraph)` (and `Start/End(Item)`)
//!   block events. Headings are excluded from paragraph text.
//!
//! - [`SegmentMode::Sentence`]: applies the paragraph pass first, then
//!   splits each paragraph on `.`, `!`, `?` terminals with:
//!   - **Abbreviation guard**: known-prefix set + single-uppercase-letter
//!     initials (e.g. "Dr.", "J.") are not split.
//!   - **Decimal guard**: a digit immediately after `.` is not a sentence end.
//!   - **Merge**: fragments shorter than [`MIN_SENTENCE_CHARS`] chars are
//!     appended to the preceding sentence (or the following one if at the start).
//!
//! ## Entry point
//!
//! [`segment_units`] is standalone — `chunk_markdown` is NOT changed in this phase.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Minimum character length for a sentence fragment before merging.
pub const MIN_SENTENCE_CHARS: usize = 12;

/// Kind of a [`TextUnit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitKind {
    Paragraph,
    Sentence,
}

/// A single extracted text unit.
#[derive(Clone, Debug, PartialEq)]
pub struct TextUnit {
    pub text: String,
    pub kind: UnitKind,
    /// Character offset of this unit's first character in the *original input*.
    /// Approximate for sentence mode (offset of the parent paragraph start).
    pub char_offset: usize,
}

/// Which segmentation granularity to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentMode {
    /// One [`TextUnit`] per markdown paragraph block.
    Paragraph,
    /// One [`TextUnit`] per sentence, using paragraph units as the input.
    Sentence,
}

// ---------------------------------------------------------------------------
// Known abbreviations (not sentence boundaries when followed by whitespace+word)
// ---------------------------------------------------------------------------

/// Lowercase-normalised abbreviations whose trailing `.` must NOT trigger a
/// sentence split. All entries are stored without trailing dot.
static KNOWN_ABBREVS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "rev", "gen", "sgt", "cpl", "pvt", "st", "dept",
    "approx", "inc", "corp", "ltd", "co", "vs", "etc", "i.e", "e.g", "fig", "no", "vol", "pp",
    "para", "avg", "max", "min", "est", "gov", "jan", "feb", "mar", "apr", "jun", "jul", "aug",
    "sep", "oct", "nov", "dec",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Segment `text` (markdown) into [`TextUnit`]s using `mode`.
///
/// - Empty or whitespace-only input → empty Vec.
/// - In [`SegmentMode::Paragraph`]: headings are excluded; each markdown
///   paragraph / list item becomes one unit.
/// - In [`SegmentMode::Sentence`]: paragraph units are further split on
///   sentence terminals with abbreviation and decimal guards; fragments
///   shorter than [`MIN_SENTENCE_CHARS`] are merged into their neighbours.
/// - Flat plain-text (no CommonMark block structure) degrades to
///   paragraph-level units (the whole text becomes one paragraph unit).
pub fn segment_units(text: &str, mode: SegmentMode) -> Vec<TextUnit> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let paragraphs = extract_paragraphs(text);

    match mode {
        SegmentMode::Paragraph => paragraphs,
        SegmentMode::Sentence => {
            let mut out = Vec::new();
            for para in paragraphs {
                let sentences = split_sentences(&para.text, para.char_offset);
                out.extend(sentences);
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Paragraph extraction (pulldown_cmark event walk)
// ---------------------------------------------------------------------------

fn extract_paragraphs(text: &str) -> Vec<TextUnit> {
    let parser = Parser::new_ext(text, Options::all());

    let mut units: Vec<TextUnit> = Vec::new();
    // Track whether we are inside a paragraph or list-item block.
    let mut in_para = false;
    let mut in_heading = false;
    // True once we encounter any markdown block event (heading, paragraph,
    // item, code block, etc.). Used to distinguish "genuinely unstructured
    // plain text" from "structured document with no paragraph content"
    // (e.g. heading-only) so the flat-text fallback does not fire for the
    // latter — Finding 3 fix.
    let mut saw_block = false;
    let mut current_text = String::new();
    // char_offset tracks position relative to the original text by consuming
    // the pulldown_cmark events' text contributions.
    //
    // TODO(phase-29): char_offset drifts when headings precede paragraphs
    // because this event-counting approach increments only for Text/Code/break
    // events and skips the characters consumed by heading syntax (e.g. "# ",
    // "## ", surrounding newlines). For a document like:
    //   "# Heading\n\nFirst paragraph."
    // the heading text "Heading" contributes 7 to char_offset, but the "#"
    // prefix (1 char), space (1), and newlines (2) are never counted, so
    // `current_offset` for "First paragraph." ends up at 7 instead of 11.
    //
    // Root cause: `Parser::new_ext` discards byte-span metadata. Fix: switch
    // `extract_paragraphs` to `Parser::new_ext(text, opts).into_offset_iter()`
    // and capture the `Paragraph` `Start` byte-range, converting to char offset
    // via `text[..start_byte].chars().count()` — the same approach used in
    // `chunk_markdown_with_headings_with_counter`. This is deferred to Phase 29
    // because it requires restructuring the event loop and the heading-only /
    // flat-text fallback paths need re-testing. Until then, `char_offset` is
    // documented as approximate (see field docs on `TextUnit`).
    let mut char_offset: usize = 0;
    let mut current_offset: usize = 0;

    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) | Event::Start(Tag::Item) => {
                saw_block = true;
                in_para = true;
                current_text.clear();
                current_offset = char_offset;
            }
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::Item) => {
                if in_para {
                    let trimmed = current_text.trim().to_string();
                    if !trimmed.is_empty() {
                        units.push(TextUnit {
                            text: trimmed,
                            kind: UnitKind::Paragraph,
                            char_offset: current_offset,
                        });
                    }
                    in_para = false;
                    current_text.clear();
                }
            }
            Event::Start(Tag::Heading { .. }) => {
                saw_block = true;
                in_heading = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
            }
            Event::Text(s) => {
                char_offset += s.chars().count();
                if in_para && !in_heading {
                    if !current_text.is_empty() && !current_text.ends_with(' ') {
                        current_text.push(' ');
                    }
                    current_text.push_str(&s);
                }
            }
            Event::Code(s) => {
                char_offset += s.chars().count();
                if in_para && !in_heading {
                    if !current_text.is_empty() && !current_text.ends_with(' ') {
                        current_text.push(' ');
                    }
                    current_text.push_str(&s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                char_offset += 1;
                if in_para && !in_heading && !current_text.is_empty() {
                    current_text.push(' ');
                }
            }
            _ => {}
        }
    }

    // Flat text with no CommonMark block structure: emit the whole text as
    // one paragraph. The `!saw_block` guard prevents firing for documents
    // that have block structure (headings, code fences, etc.) but happen to
    // contain no paragraph content — e.g. a heading-only document must yield
    // zero paragraph units, not the raw markdown source as a paragraph.
    if units.is_empty() && !saw_block && !text.trim().is_empty() {
        let trimmed = text.trim().to_string();
        units.push(TextUnit { text: trimmed, kind: UnitKind::Paragraph, char_offset: 0 });
    }

    units
}

// ---------------------------------------------------------------------------
// Sentence splitting
// ---------------------------------------------------------------------------

/// Split `paragraph_text` into sentence [`TextUnit`]s. `base_offset` is the
/// character offset of the paragraph's first character in the original source.
fn split_sentences(paragraph_text: &str, base_offset: usize) -> Vec<TextUnit> {
    // Collect candidate split positions: indices where a sentence terminal
    // (. ! ?) appears, subject to guards.
    let chars: Vec<char> = paragraph_text.chars().collect();
    let n = chars.len();

    // Collect sentence fragments by scanning char-by-char.
    let mut fragments: Vec<String> = Vec::new();
    let mut start = 0usize; // char index of current fragment start

    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        if c == '.' || c == '!' || c == '?' {
            // Check what follows the terminal (skip whitespace, then check next word).
            let after_terminal = &chars[i + 1..];

            // Find the first non-whitespace character after the terminal.
            let next_non_ws_idx = after_terminal.iter().position(|&ch| !ch.is_whitespace());

            if c == '.' {
                // Decimal guard: digit immediately after dot is not a sentence end.
                if after_terminal.first().is_some_and(|ch| ch.is_ascii_digit()) {
                    i += 1;
                    continue;
                }

                // Abbreviation guard: if the token before the dot is a known
                // abbreviation or a single uppercase letter, don't split.
                let token_before = collect_token_before(&chars, i);
                if is_abbreviation(&token_before) {
                    i += 1;
                    continue;
                }

                // End of string / no following word: split here.
                // Otherwise: split here (abbreviation guard passed).
            }

            // Emit current fragment including the terminal character.
            // Only split if there is content after (avoids trailing empty fragment).
            if next_non_ws_idx.is_some() || i + 1 == n {
                let fragment: String = chars[start..=i].iter().collect();
                fragments.push(fragment);
                // Advance start past the terminal and any trailing whitespace.
                start = i + 1;
                while start < n && chars[start].is_whitespace() {
                    start += 1;
                }
            }
        }
        i += 1;
    }

    // Flush remainder.
    if start < n {
        let tail: String = chars[start..].iter().collect();
        if !tail.trim().is_empty() {
            fragments.push(tail.trim().to_string());
        }
    }

    // If no splits found, return the whole paragraph as one sentence.
    if fragments.is_empty() {
        return vec![TextUnit {
            text: paragraph_text.trim().to_string(),
            kind: UnitKind::Sentence,
            char_offset: base_offset,
        }];
    }

    // Merge fragments shorter than MIN_SENTENCE_CHARS into neighbours.
    let merged = merge_short_fragments(fragments);

    // Build TextUnits. char_offset is approximate: paragraph start + cumulative
    // character count of preceding merged sentences.
    let mut units = Vec::with_capacity(merged.len());
    let mut cumulative = 0usize;
    for text in merged {
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        units.push(TextUnit {
            char_offset: base_offset + cumulative,
            kind: UnitKind::Sentence,
            text: text.clone(),
        });
        cumulative += text.chars().count() + 1; // +1 for separator
    }
    units
}

/// Collect the word token immediately before position `dot_pos` in `chars`.
/// Strips any leading/trailing whitespace. Returns the token in **original
/// case** so that the single-uppercase-letter initial guard in
/// [`is_abbreviation`] can inspect the true casing.
fn collect_token_before(chars: &[char], dot_pos: usize) -> String {
    // Walk backwards from dot_pos-1 to find the word boundary.
    if dot_pos == 0 {
        return String::new();
    }
    let mut end = dot_pos; // exclusive
    // Skip whitespace immediately before the dot (unusual but defensive).
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && chars[start - 1].is_alphabetic() {
        start -= 1;
    }
    // Return original case — callers that need lowercase do so explicitly.
    chars[start..end].iter().collect::<String>()
}

/// Returns true if `token` (original case) is a known abbreviation or a
/// single uppercase letter (initial, e.g. "J" in "J. Smith").
fn is_abbreviation(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // Single uppercase letter initial: "A.", "B.", etc.
    // Check original case so "J" (uppercase) is detected correctly.
    let mut word_iter = token.unicode_words();
    if word_iter.next().is_some() && word_iter.next().is_none() {
        // single-word token
        let chars: Vec<char> = token.chars().collect();
        if chars.len() == 1 && chars[0].is_uppercase() {
            return true;
        }
    }
    // Known abbreviations are stored lowercase — compare case-insensitively.
    KNOWN_ABBREVS.contains(&token.to_lowercase().as_str())
}

/// Merge fragments shorter than [`MIN_SENTENCE_CHARS`] into their neighbours.
///
/// Short leading fragment → prepend to next.
/// Short non-leading fragment → append to previous.
fn merge_short_fragments(fragments: Vec<String>) -> Vec<String> {
    if fragments.is_empty() {
        return fragments;
    }

    let mut result: Vec<String> = Vec::with_capacity(fragments.len());

    for frag in fragments {
        let trimmed = frag.trim();
        if trimmed.len() < MIN_SENTENCE_CHARS {
            if let Some(last) = result.last_mut() {
                // Append short fragment to previous.
                last.push(' ');
                last.push_str(trimmed);
            } else {
                // No previous sentence yet; buffer as pending.
                result.push(trimmed.to_string());
            }
        } else {
            // If there's a pending short fragment (first in result), merge it forward.
            if result.len() == 1 && result[0].trim().len() < MIN_SENTENCE_CHARS {
                let pending = result.remove(0);
                result.push(format!("{} {}", pending.trim(), trimmed));
            } else {
                result.push(trimmed.to_string());
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- paragraph mode ----

    #[test]
    fn paragraph_mode_yields_one_unit_per_block() {
        let md = "First paragraph.\n\nSecond paragraph with more words here.\n\nThird paragraph.";
        let units = segment_units(md, SegmentMode::Paragraph);
        assert_eq!(units.len(), 3, "expected 3 paragraphs, got {:?}", units);
        assert!(units.iter().all(|u| u.kind == UnitKind::Paragraph));
    }

    #[test]
    fn paragraph_mode_heading_text_is_excluded_or_separate() {
        let md = "# Heading\n\nParagraph text here with enough content to form a unit.";
        let units = segment_units(md, SegmentMode::Paragraph);
        // Heading text must NOT appear as a paragraph unit body (headings are excluded).
        // The paragraph should contain the prose, not "Heading".
        assert!(!units.is_empty(), "must have at least one paragraph unit");
        for u in &units {
            assert!(
                !u.text.trim_start_matches('#').trim().eq("Heading"),
                "heading text must not appear as a paragraph unit: {:?}",
                u.text
            );
        }
        let prose_unit = units.iter().find(|u| u.text.contains("Paragraph text"));
        assert!(prose_unit.is_some(), "prose paragraph must appear as a unit");
    }

    // ---- sentence mode ----

    #[test]
    fn sentence_mode_splits_on_sentence_terminal() {
        let md = "The sky is blue. The grass is green. The sun is bright.";
        let units = segment_units(md, SegmentMode::Sentence);
        assert!(
            units.len() >= 3,
            "expected at least 3 sentences, got {}: {:?}",
            units.len(),
            units
        );
        assert!(units.iter().all(|u| u.kind == UnitKind::Sentence));
    }

    #[test]
    fn sentence_mode_merges_short_fragments() {
        // "Yes." is 4 chars — shorter than MIN_SENTENCE_CHARS=12, must be merged.
        let md = "This is a normal sentence. Yes. This is another normal sentence.";
        let units = segment_units(md, SegmentMode::Sentence);
        // "Yes." must be merged into a neighbour — we should get 2 units, not 3.
        assert!(units.len() <= 2, "short fragment 'Yes.' must be merged; got {:?}", units);
        // The merged result must contain "Yes."
        let merged_text = units.iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join(" ");
        assert!(merged_text.contains("Yes"), "merged text must contain 'Yes'");
    }

    #[test]
    fn sentence_guard_abbreviation() {
        let md = "Dr. Smith went to the hospital. The patient recovered well.";
        let units = segment_units(md, SegmentMode::Sentence);
        // "Dr." must NOT trigger a split.
        // Expect 2 sentences, not 3 (Dr. Smith / went... would be wrong).
        assert_eq!(units.len(), 2, "abbreviation guard failed; got {:?}", units);
        assert!(units[0].text.contains("Dr."), "first unit must contain 'Dr.'");
    }

    #[test]
    fn sentence_guard_decimal() {
        let md = "The value is 3.14 which is pi. It is an important constant.";
        let units = segment_units(md, SegmentMode::Sentence);
        // "3.14" must NOT trigger a split on the decimal dot.
        assert_eq!(units.len(), 2, "decimal guard failed; got {:?}", units);
        assert!(units[0].text.contains("3.14"));
    }

    #[test]
    fn flat_text_degrades_to_paragraph() {
        // Plain text with no markdown block structure falls back to one paragraph unit.
        let text = "This is plain text without any markdown structure or headings.";
        let units = segment_units(text, SegmentMode::Paragraph);
        assert_eq!(units.len(), 1, "flat text must yield one paragraph unit; got {:?}", units);
        assert_eq!(units[0].kind, UnitKind::Paragraph);
    }

    #[test]
    fn empty_input_yields_empty_vec() {
        assert!(segment_units("", SegmentMode::Paragraph).is_empty());
        assert!(segment_units("   \n\n  ", SegmentMode::Paragraph).is_empty());
        assert!(segment_units("", SegmentMode::Sentence).is_empty());
    }

    /// Finding 2 guard: mid-sentence uppercase initial (e.g. "J. Smith") must
    /// NOT trigger a sentence split. The original-case token is needed for the
    /// single-uppercase-letter check; `collect_token_before` returned a
    /// lowercased string, making `chars[0].is_uppercase()` permanently false.
    #[test]
    fn mid_sentence_initial_not_split() {
        let md = "The author is named J. Smith and he wrote many books about history.";
        let units = segment_units(md, SegmentMode::Sentence);
        // "J." is a single uppercase initial — must NOT split. The whole prose
        // is one sentence (no terminal after "Smith" that merges cleanly), so
        // we expect ONE unit containing both "J." and "Smith".
        let all_text = units.iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join(" ");
        assert!(
            all_text.contains("J. Smith"),
            "initial 'J.' must not split; got units: {:?}",
            units
        );
        assert_eq!(
            units.len(),
            1,
            "mid-sentence initial must not split the sentence; got {:?}",
            units
        );
    }

    /// Finding 3 guard: a heading-only document must yield ZERO paragraph units.
    /// The flat-text fallback must only fire for genuinely unstructured plain
    /// text, not for documents that consist solely of headings.
    #[test]
    fn heading_only_doc_yields_no_paragraph_units() {
        let md = "# Only heading\n";
        let units = segment_units(md, SegmentMode::Paragraph);
        assert!(
            units.is_empty(),
            "heading-only document must yield zero paragraph units; got: {:?}",
            units
        );
    }
}
