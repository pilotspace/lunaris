//! Mechanical Stop-time citation grader (engram-soul-loop task 3).
//!
//! Pure function, NO IO / NO LLM: [`grade_injections`] turns a
//! [`crate::transcript::TurnTranscript`] into a per-memory [`MemoryVerdict`]
//! list. A memory is `cited` when its injected snippet shares at least one
//! *distinctive* n-gram (`N_GRAM` lowercased alnum tokens, stopword-filtered;
//! "distinctive" = the n-gram occurs in exactly ONE memory's snippets —
//! same-id re-injections merge before counting) with
//! the turn's final assistant message. A `post_tool` injection whose
//! attached tool call succeeded (`is_error == Some(false)`) upgrades to a
//! `ToolCall`-grain citation regardless of text overlap; a failed or
//! unattributed tool outcome falls back to the text verdict.

use std::collections::{HashMap, HashSet};

use lunaris_core::activation::Grain;
use serde::Serialize;
use ulid::Ulid;

use crate::transcript::TurnTranscript;

/// Sliding-window length (in stopword-filtered, lowercased alnum tokens)
/// used to build the "distinctive n-gram" citation signal.
pub const N_GRAM: usize = 5;

/// Per-memory citation outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Cited,
    Uncited,
}

/// One memory's Stop-time grade — the unit [`grade_injections`] returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryVerdict {
    pub id: Ulid,
    pub verdict: Verdict,
    pub grain: Grain,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// Grade every injection in `t` into a de-duplicated (one row per memory
/// id) [`MemoryVerdict`] list, in first-seen order. Pure — takes no clock,
/// no storage, no network; deterministic for a given `TurnTranscript`.
pub fn grade_injections(t: &TurnTranscript) -> Vec<MemoryVerdict> {
    let final_ngrams = ngrams(&tokenize(&t.final_assistant_text));

    // Per-injection n-gram sets, positionally aligned with `t.injections`.
    let injection_ngrams: Vec<HashSet<String>> =
        t.injections.iter().map(|m| ngrams(&tokenize(&m.snippet))).collect();

    // "Distinctive" = occurs in the snippets of exactly one MEMORY.
    // Same-id occurrences merge before counting: distinctiveness exists to
    // discriminate between memories, so a memory injected twice in one turn
    // (prompt recall + post_tool re-surface) must not demote its own
    // n-grams to non-distinctive.
    let mut ngrams_by_id: HashMap<Ulid, HashSet<&str>> = HashMap::new();
    for (i, mem) in t.injections.iter().enumerate() {
        ngrams_by_id
            .entry(mem.id)
            .or_default()
            .extend(injection_ngrams[i].iter().map(String::as_str));
    }
    let mut ngram_snippet_counts: HashMap<&str, u32> = HashMap::new();
    for set in ngrams_by_id.values() {
        for ng in set {
            *ngram_snippet_counts.entry(ng).or_insert(0) += 1;
        }
    }

    // tool_use_id -> is_error, last-write-wins if a transcript somehow
    // carries duplicate tool_result rows for the same id.
    let outcomes: HashMap<&str, Option<bool>> =
        t.tool_outcomes.iter().map(|o| (o.tool_use_id.as_str(), o.is_error)).collect();

    // rank: higher wins the per-id "best verdict" dedupe.
    // 2 = Cited/ToolCall (tool success), 1 = Cited/Turn (text match),
    // 0 = Uncited/Turn.
    let mut best: HashMap<Ulid, (u8, MemoryVerdict)> = HashMap::new();
    let mut order: Vec<Ulid> = Vec::new();

    for (i, mem) in t.injections.iter().enumerate() {
        let tool_success = matches!(
            mem.tool_use_id.as_deref().map(|id| outcomes.get(id).copied().flatten()),
            Some(Some(false))
        );

        let text_cited = injection_ngrams[i].iter().any(|ng| {
            ngram_snippet_counts.get(ng.as_str()).copied() == Some(1) && final_ngrams.contains(ng)
        });

        let (verdict, grain, rank) = if tool_success {
            (Verdict::Cited, Grain::ToolCall, 2u8)
        } else if text_cited {
            (Verdict::Cited, Grain::Turn, 1u8)
        } else {
            (Verdict::Uncited, Grain::Turn, 0u8)
        };

        let candidate =
            MemoryVerdict { id: mem.id, verdict, grain, tool_use_id: mem.tool_use_id.clone() };

        match best.get_mut(&mem.id) {
            Some((best_rank, best_verdict)) => {
                if rank > *best_rank {
                    *best_rank = rank;
                    *best_verdict = candidate;
                }
            }
            None => {
                order.push(mem.id);
                best.insert(mem.id, (rank, candidate));
            }
        }
    }

    order.into_iter().filter_map(|id| best.remove(&id).map(|(_, v)| v)).collect()
}

/// Lowercase alnum tokenization with a small closed stopword set.
/// Non-alphanumeric runs are token boundaries.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .filter(|s| !is_stopword(s))
        .collect()
}

/// Sliding-window n-grams of length [`N_GRAM`] over `tokens`, joined by a
/// single space. Empty when `tokens` is shorter than `N_GRAM`.
fn ngrams(tokens: &[String]) -> HashSet<String> {
    if tokens.len() < N_GRAM {
        return HashSet::new();
    }
    (0..=tokens.len() - N_GRAM).map(|i| tokens[i..i + N_GRAM].join(" ")).collect()
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "the", "is", "are", "was", "were", "be", "been", "being", "to", "of", "in",
    "on", "at", "for", "with", "by", "from", "as", "that", "this", "these", "those", "it", "its",
    "but", "or", "not", "no", "do", "does", "did", "has", "have", "had", "will", "would", "should",
    "can", "could", "may", "might", "must", "shall", "i", "you", "he", "she", "we", "they", "them",
    "his", "her", "their", "our", "your", "my", "me", "him", "us", "than", "then", "so", "if",
    "into", "about", "over", "under", "before", "after", "up", "down", "out", "off", "again",
    "once", "here", "there", "when", "where", "why", "how", "all", "any", "both", "each", "few",
    "more", "most", "other", "some", "such", "only", "own", "same", "too", "very", "just",
];

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{InjectedMemory, ToolOutcome};

    fn mem(id: Ulid, snippet: &str, phase: &str, tool_use_id: Option<&str>) -> InjectedMemory {
        InjectedMemory {
            id,
            snippet: snippet.to_owned(),
            phase: phase.to_owned(),
            tool_use_id: tool_use_id.map(str::to_owned),
        }
    }

    fn ulid(n: u8) -> Ulid {
        Ulid::from_bytes([n; 16])
    }

    #[test]
    fn cited_vs_uncited_by_distinctive_ngram() {
        let m1 = ulid(1);
        let m2 = ulid(2);
        let t = TurnTranscript {
            injections: vec![
                mem(
                    m1,
                    "the granite embedder resolves llamacpp inference correctly on cold start",
                    "prompt",
                    None,
                ),
                mem(m2, "coffee brewing routines and grinder settings for espresso", "prompt", None),
            ],
            tool_outcomes: vec![],
            final_assistant_text:
                "I confirmed the granite embedder resolves llamacpp inference correctly on cold start machines."
                    .to_owned(),
            session_ids_seen: HashSet::new(),
        };

        let verdicts = grade_injections(&t);
        assert_eq!(verdicts.len(), 2);
        let v1 = verdicts.iter().find(|v| v.id == m1).unwrap();
        let v2 = verdicts.iter().find(|v| v.id == m2).unwrap();
        assert_eq!(v1.verdict, Verdict::Cited);
        assert_eq!(v1.grain, Grain::Turn);
        assert_eq!(v2.verdict, Verdict::Uncited);
        assert_eq!(v2.grain, Grain::Turn);
    }

    #[test]
    fn post_tool_success_upgrades_to_toolcall_grain() {
        let m3 = ulid(3);
        let m4 = ulid(4);
        let t = TurnTranscript {
            injections: vec![
                mem(m3, "git commit created successfully", "post_tool", Some("tool-3")),
                mem(m4, "risky migration executed", "post_tool", Some("tool-4")),
            ],
            tool_outcomes: vec![
                ToolOutcome { tool_use_id: "tool-3".to_owned(), is_error: Some(false) },
                ToolOutcome { tool_use_id: "tool-4".to_owned(), is_error: Some(true) },
            ],
            final_assistant_text: "Done, unrelated wrap-up text.".to_owned(),
            session_ids_seen: HashSet::new(),
        };

        let verdicts = grade_injections(&t);
        let v3 = verdicts.iter().find(|v| v.id == m3).unwrap();
        let v4 = verdicts.iter().find(|v| v.id == m4).unwrap();
        assert_eq!(v3.verdict, Verdict::Cited);
        assert_eq!(v3.grain, Grain::ToolCall);
        assert_eq!(v3.tool_use_id.as_deref(), Some("tool-3"));
        assert_eq!(v4.verdict, Verdict::Uncited);
        assert_eq!(v4.grain, Grain::Turn);
        assert_eq!(
            v4.tool_use_id.as_deref(),
            Some("tool-4"),
            "uncited row still records tool_use_id"
        );
    }

    #[test]
    fn tool_outcome_none_stays_uncited_unless_text_cited() {
        let m5 = ulid(5);
        let t = TurnTranscript {
            injections: vec![mem(m5, "unrelated snippet text here", "post_tool", Some("tool-5"))],
            tool_outcomes: vec![ToolOutcome { tool_use_id: "tool-5".to_owned(), is_error: None }],
            final_assistant_text: "Completely different final answer text.".to_owned(),
            session_ids_seen: HashSet::new(),
        };
        let verdicts = grade_injections(&t);
        assert_eq!(verdicts[0].verdict, Verdict::Uncited);
        assert_eq!(verdicts[0].grain, Grain::Turn);
    }

    #[test]
    fn duplicate_injection_grades_once() {
        let id = ulid(6);
        let t = TurnTranscript {
            injections: vec![
                // First occurrence: uncited (unrelated text, no tool match).
                mem(id, "unrelated filler snippet content here", "prompt", None),
                // Second occurrence: post_tool with a successful outcome ->
                // strictly better (Cited/ToolCall) and must win the dedupe.
                mem(id, "unrelated filler snippet content here", "post_tool", Some("tool-6")),
            ],
            tool_outcomes: vec![ToolOutcome {
                tool_use_id: "tool-6".to_owned(),
                is_error: Some(false),
            }],
            final_assistant_text: "Some other final text unrelated to the snippet.".to_owned(),
            session_ids_seen: HashSet::new(),
        };

        let verdicts = grade_injections(&t);
        assert_eq!(verdicts.len(), 1, "same id injected twice must grade once");
        assert_eq!(verdicts[0].verdict, Verdict::Cited);
        assert_eq!(verdicts[0].grain, Grain::ToolCall);
    }

    #[test]
    fn reinjected_memory_stays_text_citable() {
        // The same memory injected TWICE (prompt recall + post_tool
        // re-surface — common in real sessions) must not demote its own
        // n-grams to non-distinctive: distinctiveness discriminates BETWEEN
        // memories, so same-id snippet sets merge before counting.
        let id = ulid(7);
        let snippet = "the granite embedder resolves llamacpp inference correctly on cold start";
        let t = TurnTranscript {
            injections: vec![
                mem(id, snippet, "prompt", None),
                mem(id, snippet, "post_tool", Some("tool-7")),
            ],
            tool_outcomes: vec![ToolOutcome {
                tool_use_id: "tool-7".to_owned(),
                is_error: Some(true),
            }],
            final_assistant_text:
                "Confirmed: the granite embedder resolves llamacpp inference correctly on cold start."
                    .to_owned(),
            session_ids_seen: HashSet::new(),
        };

        let verdicts = grade_injections(&t);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(
            verdicts[0].verdict,
            Verdict::Cited,
            "re-injecting the same memory must not make it un-citable"
        );
        assert_eq!(verdicts[0].grain, Grain::Turn);
    }

    #[test]
    fn empty_transcript_grades_to_no_verdicts() {
        let t = TurnTranscript::default();
        assert!(grade_injections(&t).is_empty());
    }
}
