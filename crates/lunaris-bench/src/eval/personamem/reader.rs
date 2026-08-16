//! PersonaMem MCQ reader — prompt rendering, reply parsing, scoring.
//!
//! Deliberately NO LLM judge: PersonaMem answers are lettered options, so the
//! score is an exact letter match against `correct_answer`. That removes the
//! ±5-point judge/gen noise floor the LongMemEval harness has to live with
//! (`reference_lme_judge_noise_floor`) — a PersonaMem A/B delta is real signal.
//!
//! Everything here is pure and unit-tested; the only I/O is the caller's chat
//! round-trip.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

/// Reader system prompt. Mirrors the benchmark's framing — a personalized
/// assistant that must apply what it remembers about THIS user — while
/// constraining the output to a single letter so scoring never needs a judge.
pub(crate) const MCQ_SYSTEM_PROMPT: &str = "You are a personalized assistant that \
    remembers this user's past conversations. You are shown memories retrieved \
    from those conversations, then the user's latest message and several \
    candidate model responses. Find the most appropriate response. Verify each \
    candidate against the memories before judging its helpfulness: a candidate \
    that makes a claim about the user (their facts, preferences, or history) \
    is wrong when the claim misstates the memories and strong when it states \
    them accurately — the most recent statement of a preference supersedes \
    older ones, and the reasons the user gave when things changed matter. When \
    the user asks for suggestions or ideas, remember you are choosing the \
    ASSISTANT's reply: a candidate narrating personal experiences in the first \
    person ('I started...', 'I created...') is someone's anecdote, not a reply \
    to the request. Prefer the suggestion that best fits this user's known \
    interests and situation — building on something they already do or love is \
    often exactly right. Reason briefly, then give your final answer as one \
    letter after the special token <final_answer>.";

/// Render the reader prompt: retrieved memories (chronological), the in-situ
/// user message, then the lettered options.
///
/// `memories` are already ordered by the caller; empty is legal (the reader
/// then answers from the options alone, which is the honest no-memory floor).
pub(crate) fn render_mcq_prompt(
    memories: &[String],
    user_message: &str,
    options: &[(char, String)],
    option_evidence: &[(char, Vec<String>)],
) -> String {
    let mut s = String::with_capacity(4096);
    s.push_str("Retrieved memories from your past conversations with this user");
    s.push_str(" (oldest first):\n");
    if memories.is_empty() {
        s.push_str("(no memories retrieved)\n");
    } else {
        for m in memories {
            s.push_str("---\n");
            s.push_str(m.trim());
            s.push('\n');
        }
    }
    s.push_str("\nThe user now says:\n");
    s.push_str(user_message.trim());
    s.push_str("\n\nCandidate model responses:\n");
    for (letter, text) in options {
        s.push_str(&format!("({letter}) {}\n", text.trim()));
    }
    // Per-option evidence: the past messages most similar to each candidate.
    // This is what lets the reader detect a RECYCLED candidate — PersonaMem's
    // fresh-idea distractors restate activities/suggestions already in the
    // history, which a topk-bounded memory window cannot reveal on its own.
    if option_evidence.iter().any(|(_, ev)| !ev.is_empty()) {
        s.push_str(
            "\nPast-conversation messages most similar to each candidate \
             (for checking novelty and factual claims):\n",
        );
        for (letter, ev) in option_evidence {
            s.push_str(&format!("({letter}):\n"));
            if ev.is_empty() {
                s.push_str("  (no similar past message)\n");
            } else {
                for m in ev {
                    s.push_str("  - ");
                    s.push_str(m.trim());
                    s.push('\n');
                }
            }
        }
    }
    // Official-protocol framing (PersonaMem inference.py): the task is to find
    // the most appropriate MODEL RESPONSE to the user's message, reasoning
    // first and emitting the letter after a `<final_answer>` token.
    //
    // The guidance is TWO-SIDED by necessity — the v2..v5 validation runs
    // mapped PersonaMem's distractor design. Recall-type questions: the gold
    // option is the one that ACCURATELY echoes the memories, and the
    // generic-helpful option is the distractor (v3's "don't trust echoes"
    // guidance broke these). Suggestion questions: gold is NEVER a
    // first-person anecdote (93/93 in the 32k split) and routinely EXTENDS an
    // activity the user already has — so novelty guidance is anti-correlated
    // with gold (v4/v5 scored below random on the category), and the working
    // signals are voice (an assistant reply addresses the user) and
    // persona-fit.
    s.push_str(
        "\nYou are the assistant in this ongoing conversation. Find the most \
         appropriate model response to the user's message. Verify each \
         candidate against the memories before judging its helpfulness: a \
         candidate that makes a claim about the user (their facts, \
         preferences, or history) is wrong when the claim misstates the \
         memories and strong when it states them accurately — the most recent \
         statement of a preference supersedes older ones, and the reasons the \
         user gave when things changed matter. When the user asks for \
         suggestions or ideas, remember you are choosing the ASSISTANT's \
         reply: a candidate narrating personal experiences in the first \
         person (\"I started...\", \"I created...\") is someone's anecdote, \
         not a reply to the request. Prefer the suggestion that best fits \
         this user's known interests and situation — building on something \
         they already do or love is often exactly right.\n\nThink it through \
         briefly, then give your final answer (",
    );
    let letters: Vec<String> = options.iter().map(|(l, _)| format!("({l})")).collect();
    s.push_str(&letters.join(", "));
    s.push_str(") after the special token <final_answer>.");
    s
}

/// Parse a model reply down to one of `valid` option letters.
///
/// Robust to the shapes readers actually emit: `"c"`, `"(c)"`, `"C."`,
/// `"Answer: (c)"`, `"The answer is C"`, a trailing letter after reasoning.
/// Returns `None` when no valid letter can be found — the caller scores that
/// as WRONG and logs it, never as a crash and never as a silent pass.
pub(crate) fn parse_letter(raw: &str, valid: &[char]) -> Option<char> {
    // Official-protocol extraction: when the reply carries `<final_answer>`,
    // only what FOLLOWS the last occurrence is the answer — reasoning text
    // before it may name several letters while weighing options.
    let scoped = match raw.rsplit_once("<final_answer>") {
        Some((_, tail)) => tail,
        None => raw,
    };
    let lower = scoped.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let is_valid = |c: char| valid.contains(&c);
    let bytes: Vec<char> = lower.chars().collect();
    // Pass 1 — a parenthesized letter `(c)` anywhere.
    for i in 0..bytes.len() {
        if bytes[i] == '(' && i + 2 < bytes.len() && bytes[i + 2] == ')' && is_valid(bytes[i + 1]) {
            return Some(bytes[i + 1]);
        }
    }
    // Pass 2 — a standalone letter token: bounded by non-alphanumerics.
    for (i, &c) in bytes.iter().enumerate() {
        if !is_valid(c) {
            continue;
        }
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after_ok = i + 1 == bytes.len() || !bytes[i + 1].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(c);
        }
    }
    None
}

/// One scored question.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PmVerdict {
    pub question_id: String,
    pub question_type: String,
    pub shared_context_id: String,
    /// Prefix end the question was allowed to see.
    pub end_index: usize,
    /// `None` when the reply carried no parseable letter (scored WRONG).
    pub predicted: Option<char>,
    pub gold: char,
    pub correct: bool,
    /// Retrieval diagnostics.
    pub hits: usize,
    pub memories: usize,
    /// Highest message index that reached the reader — MUST be `< end_index`.
    pub max_hit_index: Option<usize>,
    /// Set only on a transport/chat failure; such a question is ERR, and the
    /// tally never scores an ERR as wrong (LME H3 discipline).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One machine-readable line per question, emitted UNCONDITIONALLY so the
/// runner's tally never depends on a debug flag (LME expert-review H5).
pub(crate) fn verdict_line(v: &PmVerdict) -> String {
    let payload = serde_json::to_string(v)
        .unwrap_or_else(|e| format!("{{\"question_id\":\"?\",\"serialize_error\":\"{e}\"}}"));
    format!("PM_VERDICT {payload}")
}

/// Running accuracy with a per-question_type breakdown.
#[derive(Debug, Default)]
pub(crate) struct PmTally {
    pub correct: usize,
    pub scored: usize,
    pub errors: usize,
    by_type: BTreeMap<String, (usize, usize)>,
}

impl PmTally {
    pub(crate) fn record(&mut self, v: &PmVerdict) {
        if v.error.is_some() {
            self.errors += 1;
            return;
        }
        self.scored += 1;
        let e = self.by_type.entry(v.question_type.clone()).or_default();
        e.1 += 1;
        if v.correct {
            self.correct += 1;
            e.0 += 1;
        }
    }

    /// Accuracy in percent over SCORED questions (ERR excluded — counting a
    /// transport failure as a wrong answer silently deflates the score).
    /// Zero scored questions yields `0.0`; the caller must treat an empty
    /// window as a SKIP, never as a real 0%.
    pub(crate) fn accuracy(&self) -> f64 {
        if self.scored == 0 { 0.0 } else { 100.0 * self.correct as f64 / self.scored as f64 }
    }

    /// `(question_type, correct, scored, pct)` rows, alphabetical.
    pub(crate) fn breakdown(&self) -> Vec<(String, usize, usize, f64)> {
        self.by_type
            .iter()
            .map(|(t, (c, n))| {
                let pct = if *n == 0 { 0.0 } else { 100.0 * *c as f64 / *n as f64 };
                (t.clone(), *c, *n, pct)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<(char, String)> {
        vec![
            ('a', "first".into()),
            ('b', "second".into()),
            ('c', "third".into()),
            ('d', "fourth".into()),
        ]
    }

    #[test]
    fn prompt_carries_memories_question_and_every_option() {
        let p = render_mcq_prompt(
            &["User: I love jazz".into(), "Assistant: noted".into()],
            "Recommend me something",
            &opts(),
            &[],
        );
        assert!(p.contains("User: I love jazz"));
        assert!(p.contains("Assistant: noted"));
        assert!(p.contains("Recommend me something"));
        for (l, t) in opts() {
            assert!(p.contains(&format!("({l}) {t}")), "missing option {l}");
        }
        assert!(p.contains("<final_answer>"), "official-protocol answer token missing");
        assert!(p.contains("(a), (b), (c), (d)"));
        assert!(p.contains("most appropriate model response"), "official task framing missing");
    }

    #[test]
    fn letter_after_final_answer_token_wins_over_reasoning_mentions() {
        let valid = ['a', 'b', 'c', 'd'];
        assert_eq!(
            parse_letter(
                "Option (a) repeats the trip idea the user already heard, and (b) \
                 contradicts their latest preference. <final_answer> (c)",
                &valid
            ),
            Some('c')
        );
        // Multiple tokens: only the LAST one is authoritative.
        assert_eq!(
            parse_letter("<final_answer> (a) — wait, no. <final_answer> d", &valid),
            Some('d')
        );
        // Token present but nothing valid after it: unparseable, scored wrong.
        assert_eq!(parse_letter("<final_answer> none of them", &valid), None);
    }

    #[test]
    fn prompt_states_the_no_memory_floor_explicitly() {
        let p = render_mcq_prompt(&[], "hi", &opts(), &[]);
        assert!(p.contains("(no memories retrieved)"));
    }

    #[test]
    fn prompt_renders_per_option_evidence_when_present() {
        let ev =
            vec![('a', vec!["User: I made a scrapbook of my trips".to_string()]), ('b', vec![])];
        let p = render_mcq_prompt(&["User: hi".into()], "Any new ideas?", &opts(), &ev);
        assert!(p.contains("most similar to each candidate"));
        assert!(p.contains("User: I made a scrapbook of my trips"));
        assert!(p.contains("(no similar past message)"));
    }

    /// All-empty evidence (the no-memory floor arm) must not render the
    /// evidence section at all — the floor prompt stays byte-identical in
    /// shape to a memories-only prompt.
    #[test]
    fn prompt_omits_evidence_section_when_all_empty() {
        let ev = vec![('a', vec![]), ('b', vec![])];
        let p = render_mcq_prompt(&[], "hi", &opts(), &ev);
        assert!(!p.contains("most similar to each candidate"));
    }

    #[test]
    fn letter_parses_every_shape_a_reader_emits() {
        let valid = ['a', 'b', 'c', 'd'];
        assert_eq!(parse_letter("c", &valid), Some('c'));
        assert_eq!(parse_letter("  C  ", &valid), Some('c'));
        assert_eq!(parse_letter("(c)", &valid), Some('c'));
        assert_eq!(parse_letter("C.", &valid), Some('c'));
        assert_eq!(parse_letter("Answer: (c)", &valid), Some('c'));
        assert_eq!(parse_letter("The answer is C", &valid), Some('c'));
        assert_eq!(parse_letter("**b**", &valid), Some('b'));
    }

    /// A word starting with a valid letter must not be mistaken for the answer.
    #[test]
    fn letter_parse_ignores_letters_embedded_in_words() {
        let valid = ['a', 'b', 'c', 'd'];
        assert_eq!(parse_letter("Certainly, the best option is (d)", &valid), Some('d'));
        assert_eq!(parse_letter("Definitely believable", &valid), None);
    }

    #[test]
    fn letter_parse_failure_is_none_not_a_guess() {
        let valid = ['a', 'b', 'c', 'd'];
        assert_eq!(parse_letter("", &valid), None);
        assert_eq!(parse_letter("I don't know", &valid), None);
        assert_eq!(parse_letter("z", &valid), None);
    }

    fn verdict(qt: &str, correct: bool, error: Option<&str>) -> PmVerdict {
        PmVerdict {
            question_id: "q".into(),
            question_type: qt.into(),
            shared_context_id: "ctx".into(),
            end_index: 10,
            predicted: Some('a'),
            gold: if correct { 'a' } else { 'b' },
            correct,
            hits: 3,
            memories: 3,
            max_hit_index: Some(9),
            error: error.map(|s| s.to_string()),
        }
    }

    #[test]
    fn verdict_line_is_greppable_and_machine_readable() {
        let line = verdict_line(&verdict("recall_user_shared_facts", true, None));
        assert!(line.starts_with("PM_VERDICT "));
        let payload: serde_json::Value =
            serde_json::from_str(line.strip_prefix("PM_VERDICT ").unwrap()).unwrap();
        assert_eq!(payload["correct"], serde_json::json!(true));
        assert_eq!(payload["gold"], serde_json::json!("a"));
        assert!(payload.get("error").is_none(), "clean verdict must not carry an error key");
    }

    /// LME H3 discipline: a chat/transport failure is ERR, never WRONG.
    #[test]
    fn tally_excludes_errors_from_the_denominator() {
        let mut t = PmTally::default();
        t.record(&verdict("recall_user_shared_facts", true, None));
        t.record(&verdict("recall_user_shared_facts", false, None));
        t.record(&verdict("suggest_new_ideas", true, None));
        t.record(&verdict("suggest_new_ideas", false, Some("timeout")));
        assert_eq!(t.scored, 3);
        assert_eq!(t.correct, 2);
        assert_eq!(t.errors, 1);
        assert!((t.accuracy() - 66.666).abs() < 0.01);
    }

    #[test]
    fn tally_breaks_down_by_question_type() {
        let mut t = PmTally::default();
        t.record(&verdict("suggest_new_ideas", true, None));
        t.record(&verdict("recall_user_shared_facts", true, None));
        t.record(&verdict("recall_user_shared_facts", false, None));
        let rows = t.breakdown();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "recall_user_shared_facts");
        assert_eq!((rows[0].1, rows[0].2), (1, 2));
        assert!((rows[0].3 - 50.0).abs() < f64::EPSILON);
        assert_eq!(rows[1].0, "suggest_new_ideas");
        assert!((rows[1].3 - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_tally_reports_zero_not_nan() {
        assert_eq!(PmTally::default().accuracy(), 0.0);
    }
}
