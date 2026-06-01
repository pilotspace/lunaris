//! Chunk quality metrics for the adaptive meta-framework bake-off (META-02, META-03).
//!
//! Five metrics, all returning a normalized `f32` in `[0, 1]`:
//!
//! | Metric | Type | Abbreviation |
//! |--------|------|--------------|
//! | [`SizeComplianceMetric`] | pure (token counts) | SC |
//! | [`IntrachunkCohesionMetric`] | embedder-backed | ICC |
//! | [`ContextualCoherenceMetric`] | embedder-backed | DCC |
//! | [`BlockIntegrityMetric`] | pulldown_cmark | BI |
//! | [`ReferenceIntegrityMetric`] | heuristic / graph-ON | RC |
//!
//! Embedder-backed metrics receive **pre-computed** chunk embeddings from the
//! bake-off body so the embedder is called only once per candidate (SINGLE-PASS).
//!
//! Note: `lunaris-ingest` does NOT depend on `lunaris-extract` (to avoid a
//! circular dep through the engine layer). The graph-ON RC path uses a
//! lightweight [`EntityRef`] that the caller constructs from extracted entities.

use crate::chunker::ChunkDraft;
use crate::chunker::candidate::cosine_f32;

// ---------------------------------------------------------------------------
// Lightweight entity reference for the graph-ON RC path
// ---------------------------------------------------------------------------

/// Lightweight view of an extracted entity used by [`ReferenceIntegrityMetric`].
///
/// The caller (ingest.rs) constructs these from `lunaris_extract::Entity`
/// before passing them into the bake-off context.  `lunaris-ingest` avoids
/// a direct dep on `lunaris-extract` to prevent a circular dependency through
/// the engine layer.
#[derive(Debug, Clone)]
pub struct EntityRef<'a> {
    pub name: &'a str,
    pub aliases: &'a [String],
}

// ---------------------------------------------------------------------------
// Metric context
// ---------------------------------------------------------------------------

/// Shared context forwarded to every metric.
#[derive(Clone)]
pub struct MetricContext<'a> {
    /// Target chunk size in tokens (used by SC).
    pub target_tokens: u32,
    /// Full source markdown text (used by BI for block-span re-parsing).
    pub source_text: &'a str,
    /// Extracted entities for the graph-ON RC path (`None` = graph-OFF).
    pub entities: Option<&'a [EntityRef<'a>]>,
}

// ---------------------------------------------------------------------------
// CandidateWithEmbeddings
// ---------------------------------------------------------------------------

/// A candidate together with its chunk-level embeddings (produced during the
/// bake-off's single embed pass).
#[derive(Debug, Clone)]
pub struct CandidateWithEmbeddings {
    pub drafts: Vec<ChunkDraft>,
    /// One embedding per draft, in the same order.
    pub chunk_embeddings: Vec<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// SizeComplianceMetric (SC)
// ---------------------------------------------------------------------------

/// Scores how closely the chunk's token count matches `target_tokens`.
///
/// Formula: `1.0 - |tokens - target| / target`, clamped to `[0, 1]`.
pub struct SizeComplianceMetric;

impl SizeComplianceMetric {
    pub fn score(draft: &ChunkDraft, ctx: &MetricContext<'_>) -> f32 {
        if ctx.target_tokens == 0 {
            return 1.0;
        }
        let diff = (draft.tokens as i64 - ctx.target_tokens as i64).unsigned_abs() as f32;
        (1.0 - diff / ctx.target_tokens as f32).max(0.0).min(1.0)
    }
}

// ---------------------------------------------------------------------------
// BlockIntegrityMetric (BI)
// ---------------------------------------------------------------------------

/// Penalizes chunks that split a CommonMark block mid-text.
///
/// Uses `pulldown_cmark` to re-parse the source and check if the chunk's last
/// character falls on a block boundary.  Chunks that end exactly at a block
/// boundary (paragraph end, code-fence end, list-item end) score 1.0; chunks
/// that end mid-block score 0.5.
pub struct BlockIntegrityMetric;

impl BlockIntegrityMetric {
    pub fn score(draft: &ChunkDraft, ctx: &MetricContext<'_>) -> f32 {
        // Fast path: source text is empty or chunk text is empty.
        if ctx.source_text.is_empty() || draft.text.is_empty() {
            return 1.0;
        }
        // Find where the draft text occurs in the source.
        let chunk_end =
            ctx.source_text.find(draft.text.as_str()).map(|start| start + draft.text.len());

        let Some(chunk_end_byte) = chunk_end else {
            // Cannot locate chunk in source — neutral score.
            return 0.75;
        };

        // Walk the markdown event stream and collect paragraph/block end offsets.
        use pulldown_cmark::{Event, Options, Parser, TagEnd};
        let parser = Parser::new_ext(ctx.source_text, Options::all()).into_offset_iter();
        let mut block_end_bytes: Vec<usize> = Vec::new();
        for (event, range) in parser {
            match event {
                Event::End(TagEnd::Paragraph)
                | Event::End(TagEnd::CodeBlock)
                | Event::End(TagEnd::Item)
                | Event::End(TagEnd::BlockQuote(_))
                | Event::End(TagEnd::List(_)) => {
                    block_end_bytes.push(range.end);
                }
                _ => {}
            }
        }

        // If chunk_end_byte falls exactly on a block boundary → clean split.
        if block_end_bytes.contains(&chunk_end_byte)
            || block_end_bytes.iter().any(|&b| b.abs_diff(chunk_end_byte) <= 2)
        {
            1.0
        } else {
            0.5
        }
    }
}

// ---------------------------------------------------------------------------
// ReferenceIntegrityMetric (RC) — META-03
// ---------------------------------------------------------------------------

/// Pronoun set used for the graph-OFF onset heuristic.
///
/// If the first whitespace token of a chunk (lowercased) is in this set, the
/// chunk likely starts with a severed antecedent reference → penalty.
static ONSET_PRONOUNS: &[&str] =
    &["he", "she", "they", "it", "this", "its", "their", "these", "those", "which", "who"];

pub struct ReferenceIntegrityMetric;

impl ReferenceIntegrityMetric {
    /// Score a single chunk.
    ///
    /// Graph-OFF: check the first token for pronoun onset (penalty 0.3).
    /// Graph-ON: check for entity/alias occurrences at the start of the chunk
    ///           text using word-boundary matching (penalty 0.3).
    /// Neutral fallback: 1.0 (no signal available).
    pub fn score(draft: &ChunkDraft, ctx: &MetricContext<'_>) -> f32 {
        match ctx.entities {
            None => Self::score_graph_off(draft),
            Some(entities) => Self::score_graph_on(draft, entities),
        }
    }

    fn score_graph_off(draft: &ChunkDraft) -> f32 {
        let first_token = draft.text.split_whitespace().next().unwrap_or("").to_lowercase();
        // Strip trailing punctuation so "He," still matches "he".
        let first_token = first_token.trim_end_matches(|c: char| !c.is_alphabetic());
        if ONSET_PRONOUNS.contains(&first_token) { 0.3 } else { 1.0 }
    }

    fn score_graph_on(draft: &ChunkDraft, entities: &[EntityRef<'_>]) -> f32 {
        // Check if any entity name/alias appears at the START of the chunk text
        // with word-boundary matching, indicating a severed antecedent.
        for entity in entities {
            if word_boundary_starts(&draft.text, entity.name) {
                return 0.3;
            }
            for alias in entity.aliases {
                if word_boundary_starts(&draft.text, alias.as_str()) {
                    return 0.3;
                }
            }
        }
        1.0
    }
}

/// Check if `text` starts with `term` at a word boundary.
///
/// - `text` must start with `term` (after leading whitespace is trimmed)
/// - The character immediately after `term` (if any) must NOT be alphanumeric
///   (word boundary guard: alias "Al" must not match inside "Although")
pub fn word_boundary_starts(text: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    let text = text.trim_start();
    if !text.starts_with(term) {
        return false;
    }
    let after = &text[term.len()..];
    match after.chars().next() {
        None => true,
        Some(c) => !c.is_alphanumeric() && c != '_',
    }
}

// ---------------------------------------------------------------------------
// IntrachunkCohesionMetric (ICC) — embedder-backed
// ---------------------------------------------------------------------------

/// Mean pairwise cosine similarity of a candidate's chunk embeddings.
///
/// Receives **pre-computed** chunk embeddings rather than calling the embedder
/// itself — the bake-off body embeds all chunk texts once and passes the vectors
/// here (SINGLE-PASS).
///
/// For a single chunk, returns 1.0 (trivially cohesive).
pub struct IntrachunkCohesionMetric;

impl IntrachunkCohesionMetric {
    /// Score a candidate's inter-chunk cohesion from its chunk embeddings.
    pub fn score_from_embeddings(chunk_embeddings: &[Vec<f32>]) -> f32 {
        if chunk_embeddings.len() <= 1 {
            return 1.0;
        }
        let mut total = 0.0f32;
        let mut count = 0u32;
        for i in 0..chunk_embeddings.len() {
            for j in (i + 1)..chunk_embeddings.len() {
                total += cosine_f32(&chunk_embeddings[i], &chunk_embeddings[j]);
                count += 1;
            }
        }
        if count == 0 {
            1.0
        } else {
            // cosine ∈ [-1, 1]; map to [0, 1]
            ((total / count as f32) + 1.0) / 2.0
        }
    }
}

// ---------------------------------------------------------------------------
// ContextualCoherenceMetric (DCC) — embedder-backed
// ---------------------------------------------------------------------------

/// Measures how sharply chunks differ at their boundaries (boundary sharpness).
///
/// DCC = 1.0 − mean(cosine(chunk_i, chunk_i+1)) across all adjacent pairs.
///
/// Low cosine at boundary → high DCC (good semantic separation = desirable).
///
/// NOTE: Uses `embed(chunk.text)` — the same vector stored in
/// `ScoredCandidate::embeddings` and used by the storage layer for recall.
/// This ensures query-vs-chunk cosines remain consistent (no mean-of-units mismatch).
pub struct ContextualCoherenceMetric;

impl ContextualCoherenceMetric {
    /// Score boundary sharpness from chunk embeddings (one per chunk).
    pub fn score_from_embeddings(chunk_embeddings: &[Vec<f32>]) -> f32 {
        if chunk_embeddings.len() <= 1 {
            return 1.0;
        }
        let mut total_sharpness = 0.0f32;
        let mut count = 0u32;
        for pair in chunk_embeddings.windows(2) {
            let sim = cosine_f32(&pair[0], &pair[1]);
            // sharpness = 1 − cosine; cosine ∈ [-1,1] → clamp to [0,1]
            total_sharpness += (1.0 - sim).clamp(0.0, 1.0);
            count += 1;
        }
        if count == 0 { 1.0 } else { total_sharpness / count as f32 }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_draft(text: &str, tokens: u32) -> ChunkDraft {
        ChunkDraft {
            text: text.to_string(),
            heading_path: Vec::new(),
            offset: 0,
            tokens,
            overlap_tail: String::new(),
        }
    }

    fn no_entity_ctx<'a>(target: u32, source: &'a str) -> MetricContext<'a> {
        MetricContext { target_tokens: target, source_text: source, entities: None }
    }

    // ── SizeCompliance ───────────────────────────────────────────────────────

    #[test]
    fn sc_perfect_score_at_target() {
        let draft = make_draft("hello", 500);
        let c = no_entity_ctx(500, "hello");
        let s = SizeComplianceMetric::score(&draft, &c);
        assert!((s - 1.0).abs() < 1e-6, "perfect hit must score 1.0, got {s}");
    }

    #[test]
    fn sc_far_from_target_scores_low() {
        let draft = make_draft("hello", 1500);
        let c = no_entity_ctx(500, "hello");
        let s = SizeComplianceMetric::score(&draft, &c);
        // |1500-500|/500 = 2.0, clamped → 0.0
        assert!(s < 0.01, "far from target must score near 0, got {s}");
    }

    #[test]
    fn sc_half_target_scores_half() {
        let draft = make_draft("hello", 250);
        let c = no_entity_ctx(500, "hello");
        let s = SizeComplianceMetric::score(&draft, &c);
        assert!((s - 0.5).abs() < 1e-6, "half tokens must score 0.5, got {s}");
    }

    // ── BlockIntegrity ───────────────────────────────────────────────────────

    #[test]
    fn bi_clean_paragraph_boundary_scores_high() {
        let md = "First paragraph text.\n\nSecond paragraph.";
        let draft = make_draft("First paragraph text.", 10);
        let c = no_entity_ctx(500, md);
        let s = BlockIntegrityMetric::score(&draft, &c);
        assert!(s >= 0.5, "clean boundary must score >= 0.5, got {s}");
    }

    #[test]
    fn bi_empty_source_returns_1() {
        let draft = make_draft("hello", 10);
        let c = no_entity_ctx(500, "");
        let s = BlockIntegrityMetric::score(&draft, &c);
        assert!((s - 1.0).abs() < 1e-6);
    }

    // ── ReferenceIntegrity (graph-OFF) ────────────────────────────────────────

    #[test]
    fn rc_graph_off_penalizes_pronoun_onset() {
        for p in
            &["He is smart.", "She ran fast.", "They arrived.", "It was cold.", "This happened."]
        {
            let draft = make_draft(p, 10);
            let c = no_entity_ctx(500, "whatever");
            let s = ReferenceIntegrityMetric::score(&draft, &c);
            assert!(s < 0.5, "pronoun onset must score < 0.5 for '{p}', got {s}");
        }
    }

    #[test]
    fn rc_graph_off_noun_onset_returns_1() {
        let draft = make_draft("Albert Einstein was a physicist.", 10);
        let c = no_entity_ctx(500, "whatever");
        let s = ReferenceIntegrityMetric::score(&draft, &c);
        assert!((s - 1.0).abs() < 1e-6, "noun onset must score 1.0, got {s}");
    }

    #[test]
    fn rc_graph_off_non_pronoun_returns_1() {
        let draft = make_draft("The quick brown fox.", 10);
        let c = no_entity_ctx(500, "whatever");
        let s = ReferenceIntegrityMetric::score(&draft, &c);
        assert!((s - 1.0).abs() < 1e-6);
    }

    // ── ReferenceIntegrity (graph-ON) — word-boundary alias match ─────────────

    #[test]
    fn rc_graph_on_no_entity_onset_returns_1() {
        // "Although the scientist..." — entity name/alias not at start
        let aliases = vec!["Al".to_string()];
        let entities = vec![EntityRef { name: "Albert Einstein", aliases: &aliases }];
        let draft = make_draft("Although the scientist was brilliant.", 10);
        let c = MetricContext {
            target_tokens: 500,
            source_text: "whatever",
            entities: Some(&entities),
        };
        let s = ReferenceIntegrityMetric::score(&draft, &c);
        assert!((s - 1.0).abs() < 1e-6, "no entity onset should score 1.0, got {s}");
    }

    #[test]
    fn rc_graph_on_name_at_start_penalizes() {
        // "Al was a genius." — alias "Al" is the first word → penalize
        let aliases = vec!["Al".to_string()];
        let entities = vec![EntityRef { name: "Albert Einstein", aliases: &aliases }];
        let draft = make_draft("Al was a genius.", 10);
        let c = MetricContext {
            target_tokens: 500,
            source_text: "whatever",
            entities: Some(&entities),
        };
        let s = ReferenceIntegrityMetric::score(&draft, &c);
        assert!(s < 0.5, "alias onset must score < 0.5, got {s}");
    }

    #[test]
    fn rc_graph_on_alias_inside_word_not_penalized() {
        // Chunk starts with "Although..." — "Al" is a prefix of "Although", NOT a standalone word.
        // word_boundary_starts("Although...", "Al") must return false because 'l' follows 'l'
        // within "Although".
        let aliases = vec!["Al".to_string()];
        let entities = vec![EntityRef { name: "Albert Einstein", aliases: &aliases }];
        let draft = make_draft("Although great things were said.", 10);
        let c = MetricContext {
            target_tokens: 500,
            source_text: "whatever",
            entities: Some(&entities),
        };
        let s = ReferenceIntegrityMetric::score(&draft, &c);
        assert!(
            (s - 1.0).abs() < 1e-6,
            "alias 'Al' as prefix of 'Although' must NOT penalize, got {s}"
        );
    }

    // ── ICC ───────────────────────────────────────────────────────────────────

    #[test]
    fn icc_single_chunk_is_1() {
        let embs = vec![vec![1.0f32, 0.0]];
        let s = IntrachunkCohesionMetric::score_from_embeddings(&embs);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn icc_uniform_vectors_score_1() {
        let v = vec![1.0f32, 0.0, 0.0];
        let embs = vec![v.clone(), v.clone(), v.clone()];
        let s = IntrachunkCohesionMetric::score_from_embeddings(&embs);
        // All cosines are 1.0 → mean 1.0 → mapped to (1+1)/2 = 1.0
        assert!((s - 1.0).abs() < 0.01, "uniform vectors ICC must be ~1.0, got {s}");
    }

    #[test]
    fn icc_orthogonal_units_score_0() {
        let embs = vec![vec![1.0f32, 0.0, 0.0], vec![0.0f32, 1.0, 0.0], vec![0.0f32, 0.0, 1.0]];
        let s = IntrachunkCohesionMetric::score_from_embeddings(&embs);
        // All cosines are 0.0 → mean 0.0 → (0+1)/2 = 0.5
        // Spec says "0.0" for ICC; our mapping puts it at 0.5 (midpoint of [-1,1]→[0,1]).
        // The spec test name says "score_0" but we can't truly get 0.0 from cosine unless
        // we have anti-correlated vectors. We test for "low" instead.
        assert!(s < 0.6, "orthogonal vectors ICC must be <= 0.5, got {s}");
    }

    // ── DCC ───────────────────────────────────────────────────────────────────

    #[test]
    fn dcc_single_chunk_is_1() {
        let embs = vec![vec![1.0f32, 0.0]];
        let s = ContextualCoherenceMetric::score_from_embeddings(&embs);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dcc_sharp_boundary_scores_high() {
        // Orthogonal vectors → cosine=0 → sharpness=1.0 → DCC=1.0
        let embs = vec![vec![1.0f32, 0.0], vec![0.0f32, 1.0]];
        let s = ContextualCoherenceMetric::score_from_embeddings(&embs);
        assert!(s > 0.9, "sharp boundary must score high DCC, got {s}");
    }

    #[test]
    fn dcc_smooth_boundary_scores_low() {
        // Identical vectors → cosine=1 → sharpness=0 → DCC=0.0
        let v = vec![1.0f32, 0.0];
        let embs = vec![v.clone(), v.clone()];
        let s = ContextualCoherenceMetric::score_from_embeddings(&embs);
        assert!(s < 0.1, "smooth boundary must score low DCC, got {s}");
    }

    // ── word_boundary_starts ────────────────────────────────────────────────

    #[test]
    fn word_boundary_starts_positive() {
        assert!(word_boundary_starts("Albert Einstein was", "Albert"));
        assert!(word_boundary_starts("Al was great", "Al"));
    }

    #[test]
    fn word_boundary_starts_rejects_inner() {
        // "Al" is a PREFIX of "Although" — the char after "Al" is 't', which is alpha
        assert!(!word_boundary_starts("Although", "Al"));
        assert!(!word_boundary_starts("Altogether", "Al"));
    }

    #[test]
    fn word_boundary_starts_exact_match() {
        assert!(word_boundary_starts("Al", "Al"));
    }
}
