//! Phase 29 TREE-02 — Summarizer trait and implementations.
//!
//! ## Implementations
//!
//! - [`ExtractiveSummarizer`] — **primary tested path**. Concatenates the
//!   first sentence from each `child_text`. No external deps, always succeeds.
//!   This is the path that executes in autonomous/no-weights mode (same lesson
//!   as Phase 28 META-03: the extractive path is the live path).
//!
//! - [`LlmSummarizer`] — wraps `Arc<dyn LlmBackend>`. Gated behind the
//!   `llm-summarizer` feature so default `cargo build -p lunaris-ingest` does
//!   NOT require LLM weights. Falls back to extractive on backend error (D3).
//!
//! ## D4 guardrail
//!
//! Neither implementation touches `summary_embedding`. Callers MUST NOT set
//! `summary_embedding` from the output of `summarize()` — that is Phase 30.
//!
//! ## Line-count budget
//! This module MUST stay under 1 000 lines (per CLAUDE.md §File size).

use async_trait::async_trait;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// SummaryInput
// ---------------------------------------------------------------------------

/// Input to the [`Summarizer`] for one internal-node summary.
///
/// `child_texts` are the raw text strings of the direct-leaf descendants
/// (obtained via [`crate::chunk_texts_for_community`]). The summariser builds
/// the community summary from these.
#[derive(Clone, Debug)]
pub struct SummaryInput {
    /// The community's deterministic [`Ulid`] identifier (for correlation).
    pub node_id: Ulid,
    /// Texts of all leaf chunks that are descendants of this community node.
    pub child_texts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Summarizer trait
// ---------------------------------------------------------------------------

/// Async batch summarizer used during the bottom-up Phase 29 summarisation pass.
///
/// Inputs and outputs are parallel slices: `outputs[i]` is the summary for
/// `inputs[i]`. The output vec is always the same length as `inputs`.
///
/// Implementations MUST NOT panic on empty input or on empty `child_texts`.
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, inputs: &[SummaryInput]) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// ExtractiveSummarizer
// ---------------------------------------------------------------------------

/// Extractive summariser — no LLM required.
///
/// For each input, extracts the **first sentence** from each `child_text`
/// (up to the first `.`, `?`, `!` followed by a word boundary or end-of-text)
/// and joins them with `" "`. If no text is available, returns `"[empty section]"`.
///
/// This is the **primary tested path** in autonomous/no-weights mode.
#[derive(Clone, Debug, Default)]
pub struct ExtractiveSummarizer;

impl ExtractiveSummarizer {
    pub fn new() -> Self {
        Self
    }

    /// Extract the first sentence from `text`.
    ///
    /// Scans for a sentence-terminal character (`.`, `?`, `!`) followed by
    /// a whitespace character or end-of-string. If none is found, returns the
    /// full (trimmed) text as the "sentence" — this handles single-sentence
    /// paragraphs that lack a trailing period.
    fn first_sentence(text: &str) -> &str {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return trimmed;
        }
        let chars: Vec<char> = trimmed.chars().collect();
        for i in 0..chars.len() {
            if matches!(chars[i], '.' | '?' | '!') {
                // Lookahead: end of string or next char is whitespace.
                let is_boundary = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
                if is_boundary {
                    // Return up to and including the punctuation.
                    let byte_end: usize = trimmed
                        .char_indices()
                        .nth(i + 1)
                        .map(|(b, _)| b)
                        .unwrap_or(trimmed.len());
                    return &trimmed[..byte_end];
                }
            }
        }
        // No terminal found — return the full trimmed text.
        trimmed
    }
}

#[async_trait]
impl Summarizer for ExtractiveSummarizer {
    async fn summarize(&self, inputs: &[SummaryInput]) -> Vec<String> {
        inputs
            .iter()
            .map(|input| {
                if input.child_texts.is_empty() {
                    return "[empty section]".to_string();
                }
                let sentences: Vec<&str> = input
                    .child_texts
                    .iter()
                    .filter_map(|t| {
                        let s = Self::first_sentence(t);
                        if s.is_empty() { None } else { Some(s) }
                    })
                    .collect();
                if sentences.is_empty() {
                    "[empty section]".to_string()
                } else {
                    sentences.join(" ")
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// LlmSummarizer (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "llm-summarizer")]
pub use llm_impl::LlmSummarizer;

#[cfg(feature = "llm-summarizer")]
mod llm_impl {
    use std::sync::Arc;

    use async_trait::async_trait;
    use lunaris_llm::{GenOpts, LlmBackend, SchemaConstraint};

    use super::{ExtractiveSummarizer, Summarizer, SummaryInput};

    /// LLM-backed summariser for production use with a real language model.
    ///
    /// Falls back to [`ExtractiveSummarizer`] on any backend error so that
    /// ingest always completes (D3 fallback rule).
    ///
    /// Enabled with `--features llm-summarizer`.
    pub struct LlmSummarizer {
        backend: Arc<dyn LlmBackend>,
    }

    impl LlmSummarizer {
        pub fn new(backend: Arc<dyn LlmBackend>) -> Self {
            Self { backend }
        }

        fn build_prompt(child_texts: &[String]) -> String {
            let joined = child_texts.join(" ");
            format!("Summarize the following section in one sentence:\n{joined}")
        }
    }

    #[async_trait]
    impl Summarizer for LlmSummarizer {
        async fn summarize(&self, inputs: &[SummaryInput]) -> Vec<String> {
            let mut results = Vec::with_capacity(inputs.len());
            let extractive = ExtractiveSummarizer::new();

            for input in inputs {
                let prompt = Self::build_prompt(&input.child_texts);
                match self
                    .backend
                    .generate(&prompt, SchemaConstraint::None, GenOpts::default())
                    .await
                {
                    Ok(text) if !text.is_empty() => {
                        results.push(text);
                    }
                    Ok(_empty) => {
                        // Empty LLM response → fall back to extractive.
                        let fallback = extractive.summarize(&[input.clone()]).await;
                        results.push(fallback.into_iter().next().unwrap_or_default());
                    }
                    Err(e) => {
                        tracing::warn!(
                            node_id = %input.node_id,
                            err = %e,
                            "LlmSummarizer backend error; falling back to extractive"
                        );
                        let fallback = extractive.summarize(&[input.clone()]).await;
                        results.push(fallback.into_iter().next().unwrap_or_default());
                    }
                }
            }
            results
        }
    }
}
