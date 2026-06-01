//! Weighted argmax selector for the adaptive meta-framework bake-off (META-04).
//!
//! [`ChunkSelector::select_with_embeddings`] takes a slice of scored candidates
//! (each with pre-computed chunk embeddings) and returns the index of the winner
//! using the 5-metric weighted sum.
//!
//! ## Determinism
//!
//! The selector is deterministic: identical inputs always produce the same
//! winner index.  Tie-breaking is by candidate index (lower index wins).

use crate::chunker::metrics::{
    BlockIntegrityMetric, CandidateWithEmbeddings, ContextualCoherenceMetric,
    IntrachunkCohesionMetric, MetricContext, ReferenceIntegrityMetric, SizeComplianceMetric,
};

// ---------------------------------------------------------------------------
// SelectorWeights
// ---------------------------------------------------------------------------

/// Weights for the 5-metric weighted sum.
///
/// Default: SC=0.15, ICC=0.30, DCC=0.20, BI=0.20, RC=0.15.
/// Weights do NOT need to sum to 1.0 — the argmax is over the raw weighted sum.
#[derive(Debug, Clone)]
pub struct SelectorWeights {
    pub sc: f32,
    pub icc: f32,
    pub dcc: f32,
    pub bi: f32,
    pub rc: f32,
}

impl Default for SelectorWeights {
    fn default() -> Self {
        Self { sc: 0.15, icc: 0.30, dcc: 0.20, bi: 0.20, rc: 0.15 }
    }
}

// ---------------------------------------------------------------------------
// ChunkSelector
// ---------------------------------------------------------------------------

pub struct ChunkSelector;

impl ChunkSelector {
    /// Select the best candidate by weighted argmax.
    ///
    /// `candidates` must be non-empty.  Returns the index of the winner in
    /// `candidates`.  Tie-break: lower index wins (deterministic).
    pub fn select_with_embeddings(
        candidates: &[CandidateWithEmbeddings],
        ctx: &MetricContext<'_>,
        weights: &SelectorWeights,
    ) -> usize {
        assert!(!candidates.is_empty(), "select_with_embeddings: candidates must not be empty");

        let mut best_idx = 0usize;
        let mut best_score = f32::NEG_INFINITY;

        for (i, cand) in candidates.iter().enumerate() {
            let score = Self::score_candidate(cand, ctx, weights);
            // Tie-break: lower index wins (do NOT update on equal score)
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        best_idx
    }

    fn score_candidate(
        cand: &CandidateWithEmbeddings,
        ctx: &MetricContext<'_>,
        weights: &SelectorWeights,
    ) -> f32 {
        // SC + BI + RC: scored per-chunk, then averaged
        let n = cand.drafts.len().max(1);
        let mut sc_sum = 0.0f32;
        let mut bi_sum = 0.0f32;
        let mut rc_sum = 0.0f32;
        for draft in &cand.drafts {
            sc_sum += SizeComplianceMetric::score(draft, ctx);
            bi_sum += BlockIntegrityMetric::score(draft, ctx);
            rc_sum += ReferenceIntegrityMetric::score(draft, ctx);
        }
        let sc = sc_sum / n as f32;
        let bi = bi_sum / n as f32;
        let rc = rc_sum / n as f32;

        // ICC + DCC: scored on the full candidate's chunk embeddings
        let icc = IntrachunkCohesionMetric::score_from_embeddings(&cand.chunk_embeddings);
        let dcc = ContextualCoherenceMetric::score_from_embeddings(&cand.chunk_embeddings);

        weights.sc * sc + weights.icc * icc + weights.dcc * dcc + weights.bi * bi + weights.rc * rc
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::ChunkDraft;

    fn make_cand(texts: &[&str], tokens: &[u32]) -> CandidateWithEmbeddings {
        let drafts: Vec<ChunkDraft> = texts
            .iter()
            .zip(tokens.iter())
            .enumerate()
            .map(|(i, (t, &tok))| ChunkDraft {
                text: (*t).to_string(),
                heading_path: Vec::new(),
                offset: i as u32,
                tokens: tok,
                overlap_tail: String::new(),
            })
            .collect();
        // Use orthogonal unit vectors for embeddings so ICC/DCC are deterministic
        let dim = texts.len().max(2);
        let chunk_embeddings: Vec<Vec<f32>> = (0..texts.len())
            .map(|i| {
                let mut v = vec![0.0f32; dim];
                v[i % dim] = 1.0;
                v
            })
            .collect();
        CandidateWithEmbeddings { drafts, chunk_embeddings }
    }

    fn ctx<'a>() -> MetricContext<'a> {
        MetricContext { target_tokens: 500, source_text: "", entities: None }
    }

    #[test]
    fn selector_argmax_deterministic() {
        let cands = vec![
            make_cand(&["The quick brown fox."], &[500]),
            make_cand(&["He ran fast."], &[5]),
            make_cand(&["Albert Einstein was brilliant."], &[500]),
        ];
        let weights = SelectorWeights::default();
        let c = ctx();
        let w1 = ChunkSelector::select_with_embeddings(&cands, &c, &weights);
        let w2 = ChunkSelector::select_with_embeddings(&cands, &c, &weights);
        assert_eq!(w1, w2, "selector must be deterministic");
    }

    #[test]
    fn selector_tiebreak_by_index() {
        // Two identical candidates — lower index must win
        let weights = SelectorWeights::default();
        let c = ctx();

        // Make two candidates with identical texts/embeddings/tokens
        let cand0 = make_cand(&["Albert Einstein was a physicist."], &[500]);
        let cand1 = make_cand(&["Albert Einstein was a physicist."], &[500]);
        let cands = vec![cand0, cand1];

        let winner = ChunkSelector::select_with_embeddings(&cands, &c, &weights);
        assert_eq!(winner, 0, "tie must be broken by lower index (0), got {winner}");
    }

    #[test]
    fn selector_respects_weight_override() {
        // RC weight=1.0, all others 0.0.
        // Candidate 0: starts with "He is smart." → RC penalty → low score
        // Candidate 1: starts with "Albert Einstein was a physicist." → RC=1.0 → higher
        let weights = SelectorWeights { sc: 0.0, icc: 0.0, dcc: 0.0, bi: 0.0, rc: 1.0 };
        let cands = vec![
            make_cand(&["He is smart."], &[10]),
            make_cand(&["Albert Einstein was a physicist."], &[10]),
        ];
        let c = ctx();
        let winner = ChunkSelector::select_with_embeddings(&cands, &c, &weights);
        assert_eq!(winner, 1, "RC-dominant weight must prefer noun onset, got {winner}");
    }

    #[test]
    fn selector_max_candidates_capped_at_3() {
        // This is tested in run_bakeoff; here we just verify the selector
        // handles exactly 3 candidates without panicking.
        let cands: Vec<CandidateWithEmbeddings> =
            (0..3).map(|i| make_cand(&[&format!("Chunk {i}.")], &[500])).collect();
        let weights = SelectorWeights::default();
        let c = ctx();
        let winner = ChunkSelector::select_with_embeddings(&cands, &c, &weights);
        assert!(winner < 3);
    }
}
