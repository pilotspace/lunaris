//! Query planner stub — RETRIEVE-13.
//!
//! v0 ships a heuristic that inspects the query text for entity-like
//! capitalized tokens and returns `Plan::Hybrid` (vector + keyword) when
//! present, `Plan::VectorOnly` otherwise. Phase 3 will extend with
//! `Plan::HybridGraph` once the graph extractor lands.
//!
//! ## Heuristic
//!
//! Walk whitespace-delimited tokens; a token is "entity-like" if it starts with
//! an uppercase ASCII letter AND it is NOT the first token of the query
//! (sentence-initial capitalization is not a signal). When at least one such
//! token is found, return `Plan::Hybrid`. Otherwise `Plan::VectorOnly`.
//!
//! v0 caveat: this is an English-only heuristic. Multilingual / case-less
//! scripts (Chinese, Japanese, Korean) always pick `VectorOnly` — Phase 3
//! replaces this with the full graph-anchored extractor.

use serde::{Deserialize, Serialize};

/// Retrieval plan picked by the v0 query planner.
///
/// `VectorOnly` and `Hybrid` are the only v0 variants; Phase 3 will extend
/// with `HybridGraph` once the graph extractor is wired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Plan {
    /// Vector search only — sufficient for purely semantic queries with no
    /// named entities.
    VectorOnly,
    /// Vector + keyword (BM25) hybrid — appropriate when the query mentions
    /// entity-like capitalized tokens that BM25 can match exactly.
    Hybrid,
}

/// Run the v0 planner heuristic.
pub fn plan_query(text: &str) -> Plan {
    if has_entity_like_capitalized_token(text) { Plan::Hybrid } else { Plan::VectorOnly }
}

/// Walk whitespace-delimited tokens; return `true` when at least one
/// non-first token starts with an uppercase ASCII letter.
fn has_entity_like_capitalized_token(text: &str) -> bool {
    text.split_whitespace().enumerate().any(|(i, tok)| {
        if i == 0 {
            return false;
        }
        // strip leading punctuation that isn't a letter (e.g., "(Alice")
        let first = tok.chars().find(|c| c.is_ascii_alphanumeric());
        matches!(first, Some(c) if c.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_hybrid_for_capitalized_entity_token() {
        assert_eq!(plan_query("what did Alice do at Acme last April?"), Plan::Hybrid);
    }

    #[test]
    fn picks_vector_only_for_lowercase_phrase() {
        assert_eq!(plan_query("show me everything"), Plan::VectorOnly);
        assert_eq!(plan_query("how can i find this thing"), Plan::VectorOnly);
    }

    #[test]
    fn first_word_capitalization_is_not_a_signal() {
        // "What" is sentence-initial — should NOT trigger hybrid.
        assert_eq!(plan_query("What is going on"), Plan::VectorOnly);
    }

    #[test]
    fn punctuation_prefix_is_skipped() {
        // "(Alice" — leading paren skipped, "A" still triggers.
        assert_eq!(plan_query("hello (Alice) how are you"), Plan::Hybrid);
    }

    #[test]
    fn empty_query_is_vector_only() {
        assert_eq!(plan_query(""), Plan::VectorOnly);
    }
}
