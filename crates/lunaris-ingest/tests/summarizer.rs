//! Phase 29 Plan 02 — Summarizer trait tests (Fixture 3 setup).
//!
//! RED phase: all tests fail because `Summarizer`, `ExtractiveSummarizer`,
//! and `SummaryInput` do not yet exist in lunaris-ingest.

use lunaris_ingest::{ExtractiveSummarizer, Summarizer, SummaryInput};
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Extractive path tests
// ---------------------------------------------------------------------------

/// ExtractiveSummarizer produces non-empty strings from child texts.
#[tokio::test]
async fn extractive_basic() {
    let summarizer = ExtractiveSummarizer::new();
    let inputs = vec![
        SummaryInput {
            node_id: Ulid::new(),
            child_texts: vec![
                "Hello world. More text here.".into(),
                "Second child sentence.".into(),
            ],
        },
        SummaryInput {
            node_id: Ulid::new(),
            child_texts: vec!["Another section. Extra content.".into()],
        },
    ];
    let summaries = summarizer.summarize(&inputs).await;
    assert_eq!(summaries.len(), 2, "output length must match input length");
    assert!(!summaries[0].is_empty(), "summary[0] must be non-empty");
    assert!(!summaries[1].is_empty(), "summary[1] must be non-empty");
    // First sentence extraction: should contain text from the first child.
    assert!(
        summaries[0].contains("Hello world") || summaries[0].contains("Hello"),
        "summary[0] should include first sentence of first child; got: {:?}",
        summaries[0]
    );
}

/// Single child with multi-sentence text: only the first sentence is used.
#[tokio::test]
async fn extractive_single_child_first_sentence_only() {
    let summarizer = ExtractiveSummarizer::new();
    let input = SummaryInput {
        node_id: Ulid::new(),
        child_texts: vec!["First sentence. Second sentence. Third sentence.".into()],
    };
    let summaries = summarizer.summarize(&[input]).await;
    assert_eq!(summaries.len(), 1);
    let s = &summaries[0];
    // Should contain first sentence but NOT the full multi-sentence text.
    assert!(!s.is_empty(), "summary must not be empty");
    // First sentence boundary check: should include "First sentence"
    assert!(
        s.contains("First sentence"),
        "extractive summary should include first sentence; got: {s:?}"
    );
}

/// Empty child list produces a non-empty fallback — never panics.
#[tokio::test]
async fn extractive_empty_child_list_fallback() {
    let summarizer = ExtractiveSummarizer::new();
    let input = SummaryInput { node_id: Ulid::new(), child_texts: vec![] };
    let summaries = summarizer.summarize(&[input]).await;
    assert_eq!(summaries.len(), 1);
    assert!(
        !summaries[0].is_empty(),
        "empty-child summary must not be empty (fallback to '[empty section]')"
    );
}

// ---------------------------------------------------------------------------
// LLM faux path tests (gated behind llm-summarizer feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "llm-summarizer")]
mod llm_faux {
    use std::sync::Arc;

    use lunaris_ingest::{LlmSummarizer, Summarizer, SummaryInput};
    use lunaris_llm::faux::FauxBackend;
    use ulid::Ulid;

    /// LlmSummarizer returns the queued LLM response on success.
    #[tokio::test]
    async fn llm_faux_returns_queued_response() {
        let backend = Arc::new(FauxBackend::new().with_response("Generated summary."));
        let summarizer = LlmSummarizer::new(backend);
        let input =
            SummaryInput { node_id: Ulid::new(), child_texts: vec!["Some text here.".into()] };
        let summaries = summarizer.summarize(&[input]).await;
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0], "Generated summary.");
    }

    /// On LlmBackend error, LlmSummarizer falls back to extractive — output non-empty.
    #[tokio::test]
    async fn llm_faux_falls_back_to_extractive_on_error() {
        let backend = Arc::new(FauxBackend::new().with_error_msg("simulated LLM failure"));
        let summarizer = LlmSummarizer::new(backend);
        let input =
            SummaryInput { node_id: Ulid::new(), child_texts: vec!["Fallback text here.".into()] };
        let summaries = summarizer.summarize(&[input]).await;
        assert_eq!(summaries.len(), 1);
        assert!(
            !summaries[0].is_empty(),
            "fallback to extractive must produce non-empty summary; got: {:?}",
            summaries[0]
        );
    }
}
