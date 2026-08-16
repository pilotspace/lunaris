//! PersonaMem dataset parsing — pure, I/O-free, unit-tested.
//!
//! ## Verified schema (2026-08-16, HF `bowen-upenn/PersonaMem`, MIT)
//!
//! `questions_{SIZE}.csv` (SIZE ∈ `32k` | `128k` | `1M`) — RFC-4180 CSV whose
//! text fields carry embedded commas, quotes and newlines. Columns (32k split:
//! 589 rows, 37 distinct `shared_context_id`):
//!
//! | column | shape |
//! |---|---|
//! | `persona_id` | `"0"` |
//! | `question_id` | uuid |
//! | `question_type` | one of 7 labels (`recall_user_shared_facts`, …) |
//! | `topic` | `"musicRecommendation"` |
//! | `user_question_or_message` | the user turn to answer |
//! | `correct_answer` | **`"(c)"`** — parenthesized letter, never prose |
//! | `all_options` | list of 4 `"(a) …"` strings — see below |
//! | `shared_context_id` | join key into the JSONL |
//! | `end_index_in_shared_context` | EXCLUSIVE slice end: `context[:end]` |
//!
//! (plus `context_length_in_{tokens,letters}`,
//! `distance_to_ref_in_{blocks,tokens}`, `num_irrelevant_tokens`,
//! `distance_to_ref_proportion_in_context` — diagnostics this harness ignores.)
//!
//! **`all_options` is NOT reliably JSON.** On the 32k split 286/589 rows are
//! valid JSON arrays and **303/589 are Python `repr` lists** (single-quoted,
//! `\'`-escaped). A `serde_json`-only parser silently drops 51% of the
//! benchmark. [`parse_options`] therefore ignores the container syntax and
//! splits on the `<quote>(x)` option markers, which is exactly equivalent to
//! the JSON parse on every row that parses as JSON (verified 589/589).
//!
//! `shared_contexts_{SIZE}.jsonl` — one JSON object per line, exactly one key
//! (the `shared_context_id`) whose value is the message list
//! `[{"role": "system"|"user"|"assistant", "content": "…"}, …]`. 32k split: 37
//! lines, 116–238 messages each. `system` messages repeat the SAME persona
//! blob at every session boundary (5 occurrences, 1 distinct) — deduped by
//! [`IngestCursor`]. Most `user`/`assistant` contents are already prefixed
//! `"User: "` / `"Assistant: "`, but not all (246/6267 are bare), so
//! [`render_message`] normalizes.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

/// One PersonaMem multiple-choice question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PmQuestion {
    pub persona_id: String,
    pub question_id: String,
    pub question_type: String,
    pub topic: String,
    /// `user_question_or_message` — the in-situ user turn the reader answers.
    pub user_message: String,
    /// Normalized gold option letter (`'c'`), parsed from `"(c)"`.
    pub gold_letter: char,
    /// `(letter, option_text)` in dataset order, letters lowercased.
    pub options: Vec<(char, String)>,
    pub shared_context_id: String,
    /// EXCLUSIVE prefix end: the question may only see `messages[..end_index]`.
    pub end_index: usize,
}

/// One `{role, content}` message of a shared context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PmMessage {
    pub role: String,
    pub content: String,
}

/// All questions sharing one context, ordered by the prefix they may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PmContextGroup {
    pub shared_context_id: String,
    /// Sorted by `(end_index, question_id)` — the incremental-ingest order.
    pub questions: Vec<PmQuestion>,
}

/// Split RFC-4180 CSV text into records. Handles quoted fields containing
/// commas, doubled `""` escapes and embedded newlines; `\r\n` line endings
/// normalize to `\n` outside quotes.
pub(crate) fn parse_csv_records(input: &str) -> Vec<Vec<String>> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Parse an `all_options` cell into `(letter, text)` pairs.
///
/// Container-syntax agnostic (see the module doc): scans for `<quote>(x)`
/// option markers and takes the text between consecutive markers. Returns an
/// empty vec when no marker is present.
pub(crate) fn parse_options(raw: &str) -> Vec<(char, String)> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Trim the list-container residue (`', ` / `']` / `", ` / `"]`) off the tail
/// of one option segment.
fn strip_option_tail(seg: &str) -> &str {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Undo the Python-`repr` / JSON escapes that survive the marker split.
/// Unknown escapes keep their backslash rather than being silently eaten.
fn unescape_repr(s: &str) -> String {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Normalize a `correct_answer` / model reply fragment to a bare option
/// letter: `"(c)"`, `" C "`, `"c)"` → `'c'`. `None` when no ASCII letter is
/// present.
pub(crate) fn normalize_letter(s: &str) -> Option<char> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Parse `questions_{SIZE}.csv` bytes. Rows missing a required column value,
/// carrying an unparseable `end_index_in_shared_context`, an unreadable
/// `correct_answer`, or fewer than two options are SKIPPED with a stderr note
/// — one malformed row must never abort a benchmark run.
pub(crate) fn parse_questions_csv(bytes: &[u8]) -> anyhow::Result<Vec<PmQuestion>> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Group questions by `shared_context_id`, preserving first-appearance order
/// of the contexts (so `LUNARIS_EVAL_PM_OFFSET` is stable across runs) and
/// sorting each group by `(end_index, question_id)` — the order in which the
/// incremental prefix ingest can answer them.
pub(crate) fn group_by_context(questions: Vec<PmQuestion>) -> Vec<PmContextGroup> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Parse `shared_contexts_{SIZE}.jsonl`, keeping only the ids in `wanted`
/// (empty `wanted` = keep everything). Streaming per line so the 1M split's
/// multi-hundred-MB file never materializes as one `serde_json::Value`.
/// A malformed line is skipped, not fatal.
pub(crate) fn parse_contexts_jsonl(
    text: &str,
    wanted: &HashSet<String>,
) -> HashMap<String, Vec<PmMessage>> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Render one message as an ingestable document body. `system` messages are
/// the persona blob and pass through verbatim; `user` / `assistant` messages
/// get their speaker prefix only when the dataset did not already carry one.
pub(crate) fn render_message(msg: &PmMessage) -> String {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Document key for message `idx` — zero-padded so `Hit::source` sorts and
/// parses back (see [`index_from_source`]).
pub(crate) fn doc_key(idx: usize) -> String {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// Recover the message index from a `Hit::source`
/// (`helios:fs/<session>/m00042.md`). `None` for any other shape.
pub(crate) fn index_from_source(source: &str) -> Option<usize> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

/// **Temporal honesty, enforced by construction.**
///
/// A PersonaMem question may only see `messages[..end_index]`. The cursor
/// walks each shared context ONCE, strictly forward, emitting each message
/// index at most once and NEVER an index `>= end`. Because the store is only
/// ever appended to (no document is rewritten), recall for a question with
/// prefix end `e` can physically only surface messages `< e` — later context
/// has not been written yet.
///
/// Exactly-duplicated document bodies (PersonaMem restates the whole persona
/// `system` blob at every session boundary — 5 copies, 1 distinct) are emitted
/// once; a re-ingest would only add rank-competing near-duplicates.
#[derive(Debug, Default)]
pub(crate) struct IngestCursor {
    next: usize,
    seen: HashSet<String>,
}

impl IngestCursor {
    pub(crate) fn new() -> Self {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

    /// How far the store has been advanced (exclusive message index).
    pub(crate) fn position(&self) -> usize {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }

    /// Documents to write so the store reflects `messages[..end]`.
    /// `end` is clamped to `messages.len()`; an `end` at or below the current
    /// position yields nothing (never rewinds, never rewrites).
    pub(crate) fn advance(&mut self, messages: &[PmMessage], end: usize) -> Vec<(usize, String)> {
        unimplemented!("RED: PersonaMem harness not implemented yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> PmMessage {
        PmMessage { role: role.into(), content: content.into() }
    }

    #[test]
    fn csv_reader_handles_quotes_commas_and_embedded_newlines() {
        let input = "a,b,c\n1,\"x, y\",\"line1\nline2\"\n2,\"he said \"\"hi\"\"\",z\n";
        let recs = parse_csv_records(input);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0], vec!["a", "b", "c"]);
        assert_eq!(recs[1], vec!["1", "x, y", "line1\nline2"]);
        assert_eq!(recs[2], vec!["2", "he said \"hi\"", "z"]);
    }

    #[test]
    fn csv_reader_tolerates_crlf_and_missing_trailing_newline() {
        let recs = parse_csv_records("a,b\r\n1,2");
        assert_eq!(recs, vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    #[test]
    fn options_parse_from_json_serialized_rows() {
        let raw = r#"["(a) first", "(b) second", "(c) third", "(d) fourth"]"#;
        assert_eq!(
            parse_options(raw),
            vec![
                ('a', "first".to_string()),
                ('b', "second".to_string()),
                ('c', "third".to_string()),
                ('d', "fourth".to_string()),
            ]
        );
    }

    /// 303/589 rows of the 32k split serialize `all_options` as a Python repr
    /// list. A `serde_json`-only parser drops all of them silently.
    #[test]
    fn options_parse_from_python_repr_rows() {
        let raw = "['(a) it\\'s fine', '(b) second, with comma', '(c) third', '(d) fourth']";
        assert_eq!(
            parse_options(raw),
            vec![
                ('a', "it's fine".to_string()),
                ('b', "second, with comma".to_string()),
                ('c', "third".to_string()),
                ('d', "fourth".to_string()),
            ]
        );
    }

    #[test]
    fn options_parse_returns_empty_for_unmarked_cell() {
        assert!(parse_options("no options here").is_empty());
    }

    #[test]
    fn letter_normalizes_from_parenthesized_gold() {
        assert_eq!(normalize_letter("(c)"), Some('c'));
        assert_eq!(normalize_letter(" D) "), Some('d'));
        assert_eq!(normalize_letter("b"), Some('b'));
        assert_eq!(normalize_letter("()"), None);
    }

    #[test]
    fn questions_csv_parses_verified_schema() {
        let csv = concat!(
            "persona_id,question_id,question_type,topic,user_question_or_message,",
            "correct_answer,all_options,shared_context_id,end_index_in_shared_context\n",
            "0,q1,recall_user_shared_facts,music,\"Hi, there\",(c),",
            "\"[\"\"(a) one\"\", \"\"(b) two\"\", \"\"(c) three\"\", \"\"(d) four\"\"]\",ctxA,182\n"
        );
        let qs = parse_questions_csv(csv.as_bytes()).unwrap();
        assert_eq!(qs.len(), 1);
        let q = &qs[0];
        assert_eq!(q.question_id, "q1");
        assert_eq!(q.question_type, "recall_user_shared_facts");
        assert_eq!(q.user_message, "Hi, there");
        assert_eq!(q.gold_letter, 'c');
        assert_eq!(q.options.len(), 4);
        assert_eq!(q.shared_context_id, "ctxA");
        assert_eq!(q.end_index, 182);
    }

    #[test]
    fn questions_csv_skips_malformed_rows_instead_of_failing() {
        let csv = concat!(
            "question_id,question_type,user_question_or_message,correct_answer,",
            "all_options,shared_context_id,end_index_in_shared_context\n",
            "good,t,msg,(a),\"['(a) x', '(b) y']\",ctxA,5\n",
            "badend,t,msg,(a),\"['(a) x', '(b) y']\",ctxA,not-a-number\n",
            "goldmissing,t,msg,(z),\"['(a) x', '(b) y']\",ctxA,5\n"
        );
        let qs = parse_questions_csv(csv.as_bytes()).unwrap();
        assert_eq!(qs.iter().map(|q| q.question_id.as_str()).collect::<Vec<_>>(), vec!["good"]);
    }

    #[test]
    fn questions_csv_missing_required_column_is_an_error() {
        assert!(parse_questions_csv(b"question_id,topic\nq1,music\n").is_err());
    }

    #[test]
    fn contexts_jsonl_filters_to_wanted_ids() {
        let text = concat!(
            "{\"ctxA\": [{\"role\": \"system\", \"content\": \"persona\"}]}\n",
            "{\"ctxB\": [{\"role\": \"user\", \"content\": \"User: hi\"}]}\n",
            "not json\n"
        );
        let wanted: HashSet<String> = ["ctxB".to_string()].into_iter().collect();
        let got = parse_contexts_jsonl(text, &wanted);
        assert_eq!(got.len(), 1);
        assert_eq!(got["ctxB"], vec![msg("user", "User: hi")]);
    }

    #[test]
    fn render_message_adds_speaker_prefix_only_when_absent() {
        assert_eq!(render_message(&msg("user", "User: hi")), "User: hi");
        assert_eq!(render_message(&msg("user", "hi")), "User: hi");
        assert_eq!(render_message(&msg("assistant", "sure")), "Assistant: sure");
        assert_eq!(render_message(&msg("system", "Current user persona: X")), "Current user persona: X");
    }

    #[test]
    fn doc_key_round_trips_through_a_hit_source() {
        assert_eq!(doc_key(42), "m00042.md");
        assert_eq!(index_from_source("helios:fs/pm0003/m00042.md"), Some(42));
        assert_eq!(index_from_source("fact:01H"), None);
    }

    /// TEMPORAL HONESTY. The cursor must never hand the store a message at or
    /// beyond the question's `end_index`, so recall physically cannot see the
    /// future — this is the invariant the whole harness rests on.
    #[test]
    fn ingest_cursor_never_emits_at_or_past_the_prefix_end() {
        let msgs: Vec<PmMessage> = (0..10).map(|i| msg("user", &format!("turn {i}"))).collect();
        let mut cur = IngestCursor::new();
        let first = cur.advance(&msgs, 4);
        assert_eq!(first.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert!(first.iter().all(|(i, _)| *i < 4));
        assert_eq!(cur.position(), 4);

        let second = cur.advance(&msgs, 7);
        assert_eq!(second.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![4, 5, 6]);
        assert!(second.iter().all(|(i, _)| *i < 7));
    }

    #[test]
    fn ingest_cursor_never_rewinds_or_rewrites() {
        let msgs: Vec<PmMessage> = (0..5).map(|i| msg("user", &format!("turn {i}"))).collect();
        let mut cur = IngestCursor::new();
        assert_eq!(cur.advance(&msgs, 3).len(), 3);
        // A question with an EARLIER prefix end re-emits nothing.
        assert!(cur.advance(&msgs, 2).is_empty());
        assert_eq!(cur.position(), 3);
        // Same end => nothing.
        assert!(cur.advance(&msgs, 3).is_empty());
        // Clamped to the message count.
        assert_eq!(cur.advance(&msgs, 99).len(), 2);
    }

    #[test]
    fn ingest_cursor_dedups_the_repeated_persona_block() {
        let msgs = vec![
            msg("system", "PERSONA"),
            msg("user", "User: a"),
            msg("system", "PERSONA"),
            msg("user", "User: b"),
        ];
        let mut cur = IngestCursor::new();
        let docs = cur.advance(&msgs, 4);
        assert_eq!(docs.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 3]);
        assert_eq!(cur.position(), 4);
    }

    #[test]
    fn grouping_preserves_context_order_and_sorts_by_prefix_end() {
        let mk = |qid: &str, ctx: &str, end: usize| PmQuestion {
            persona_id: "0".into(),
            question_id: qid.into(),
            question_type: "t".into(),
            topic: "x".into(),
            user_message: "m".into(),
            gold_letter: 'a',
            options: vec![('a', "x".into()), ('b', "y".into())],
            shared_context_id: ctx.into(),
            end_index: end,
        };
        let groups = group_by_context(vec![
            mk("q1", "ctxB", 20),
            mk("q2", "ctxA", 30),
            mk("q3", "ctxB", 10),
            mk("q4", "ctxA", 5),
        ]);
        assert_eq!(
            groups.iter().map(|g| g.shared_context_id.as_str()).collect::<Vec<_>>(),
            vec!["ctxB", "ctxA"]
        );
        assert_eq!(
            groups[0].questions.iter().map(|q| q.end_index).collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(groups[1].questions.iter().map(|q| q.end_index).collect::<Vec<_>>(), vec![5, 30]);
    }
}
