//! [`NoopExtractor`] — passthrough that emits empty extractions.
//!
//! Used as the default when:
//!
//! - The graph pipeline is OFF (Plan 03-03; default per blueprint §5.2 + D-11),
//!   in which case `Lunaris::ingest` short-circuits before calling `extract`
//!   anyway. The Noop seam keeps the trait-object slot full so the umbrella
//!   handle's invariant `extractor: Arc<dyn Extractor>` always holds.
//! - The candle backend cache is missing (Plan 03-03 calls
//!   `CandleGemma3_4B::try_new_from_default_cache()` and on `Err` substitutes
//!   `Arc::new(NoopExtractor)` with `tracing::warn!`). Mirrors the Plan 02-03
//!   `BgeRerankerV2M3 → NoopReranker` cache-miss pattern.
//!
//! `applies()` returns `false` so callers (Plan 03-03 ingest fan-out) can
//! short-circuit before walking the WriteOp builder loop.

use async_trait::async_trait;
use lunaris_core::LunarisError;
use ulid::Ulid;

use crate::Extractor;
use crate::types::{ChunkInput, RawExtraction, RawExtractionBatch};

/// Default extractor when graph pipeline is OFF (Plan 03-03) or when the candle
/// backend cache is missing. Returns one empty [`RawExtraction`] per input
/// chunk so the per-chunk index alignment downstream still holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopExtractor;

#[async_trait]
impl Extractor for NoopExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        Ok(RawExtractionBatch {
            by_chunk: chunks
                .iter()
                .map(|c| RawExtraction {
                    source_chunk_id: c.chunk_id,
                    entities: Vec::new(),
                    relations: Vec::new(),
                    facts: Vec::new(),
                })
                .collect(),
        })
    }

    fn applies(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_returns_empty_extractions() {
        let chunks = vec![
            ChunkInput {
                chunk_id: Ulid::new(),
                text: "hello world".into(),
                heading_path: vec!["intro".into()],
            },
            ChunkInput { chunk_id: Ulid::new(), text: "second chunk".into(), heading_path: vec![] },
        ];
        let extracted = NoopExtractor.extract(Ulid::new(), &chunks).await.unwrap();
        assert_eq!(extracted.by_chunk.len(), 2);
        for (i, r) in extracted.by_chunk.iter().enumerate() {
            assert_eq!(r.source_chunk_id, chunks[i].chunk_id);
            assert!(r.entities.is_empty());
            assert!(r.relations.is_empty());
            assert!(r.facts.is_empty());
        }
    }

    #[tokio::test]
    async fn noop_handles_empty_input_batch() {
        let extracted = NoopExtractor.extract(Ulid::new(), &[]).await.unwrap();
        assert!(extracted.by_chunk.is_empty());
    }

    #[test]
    fn noop_applies_is_false() {
        assert!(!NoopExtractor.applies(), "NoopExtractor::applies must be false");
    }
}
