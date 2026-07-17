//! `NoopReranker` — passthrough fallback for the RETRIEVE-06 contract.
//!
//! Returns input docs unchanged in original order with original scores.
//! The DSL operator wraps every rerank call with this fallback when the
//! configured `BgeRerankerV2M3` weights are unavailable, so the recall path
//! still runs end-to-end (just without the 12 ms cross-encoder relevance
//! boost). `applies()` returns `false` so callers can see via
//! `Hit { rerank_applied: false }` that they got the degraded path.

use async_trait::async_trait;
use lunaris_core::LunarisError;

use crate::{RerankCandidate, Reranker};

/// NO-OP reranker. Used as the default fallback when `crate::BgeRerankerV2M3`
/// weights are missing. Production code wires this via
/// `Lunaris::open(url)` automatically when the cache is empty.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        // Preserve input order — the DSL operator preserves the upstream
        // ranking when the reranker is a no-op so callers don't see a
        // mysterious re-shuffle on cache-miss boxes.
        Ok(docs)
    }

    fn applies(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cand(id: &[u8], score: f32) -> RerankCandidate {
        RerankCandidate {
            id: id.to_vec(),
            text: format!("doc-{}", String::from_utf8_lossy(id)),
            score,
            metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn noop_returns_input_unchanged() {
        let docs = vec![
            cand(b"a", 0.1),
            cand(b"b", 0.5),
            cand(b"c", 0.3),
            cand(b"d", 0.9),
            cand(b"e", 0.2),
        ];
        let out = NoopReranker.rerank("any query", docs).await.unwrap();
        // Order MUST be preserved (NoopReranker does NOT re-sort).
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].id, b"a".to_vec());
        assert_eq!(out[1].id, b"b".to_vec());
        assert_eq!(out[2].id, b"c".to_vec());
        assert_eq!(out[3].id, b"d".to_vec());
        assert_eq!(out[4].id, b"e".to_vec());
        // Scores MUST be preserved verbatim.
        assert!((out[0].score - 0.1).abs() < 1e-6);
        assert!((out[3].score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn noop_applies_is_false() {
        assert!(!NoopReranker.applies());
    }
}
