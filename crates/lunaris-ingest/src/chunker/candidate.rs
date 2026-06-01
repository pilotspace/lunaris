//! Candidate generator trait + four generator implementations (META-01).
//!
//! ## Architecture
//!
//! The bake-off runs [`MAX_BAKEOFF_CANDIDATES`] candidate chunk lists through
//! [`run_bakeoff`], which:
//! 1. Collects up to `max_candidates` `Vec<ChunkDraft>` from the generators,
//!    dropping any that `Err` (logged via `tracing::warn!`).
//! 2. Ensures the structural generator is always present (the fallback floor).
//! 3. Embeds every candidate's chunk texts in ONE batch each → stores them in
//!    [`ScoredCandidate::embeddings`] so the selector **and** the ingest pipeline
//!    both use the SAME vectors (SINGLE-PASS invariant).
//! 4. Hands all candidates to the selector (metrics + weighted argmax).
//! 5. Returns the winner as a [`ScoredCandidate`].
//!
//! ## SINGLE-PASS invariant
//!
//! [`ScoredCandidate::embeddings`] carries the chunk-level embeddings produced
//! during scoring.  The storage layer MUST use them directly — it must NOT call
//! `embed_batch` on the winner again.  See the `// SINGLE-PASS:` comment on the
//! struct field.
//!
//! ## Object-safety
//!
//! [`CandidateGenerator`] is intentionally **sync** (no `async fn`) so it is
//! dyn-compatible without `#[async_trait]` boxing on MSRV 1.94.  Generators that
//! need to embed (e.g. [`SemanticBreakpointGenerator`]) receive pre-computed unit
//! embeddings from `run_bakeoff`; they do NOT call the embedder themselves.

use lunaris_core::{Embedder, LunarisError, StorageError};

use crate::chunker::segment::SegmentMode;
use crate::chunker::{
    ChunkDraft, HeadingRecord, TokenCounter, chunk_markdown_with_headings_with_counter,
    segment_units,
};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// One candidate chunk list produced by a [`CandidateGenerator`], together
/// with the chunk-level embeddings that were computed during metric scoring.
///
/// # SINGLE-PASS
///
/// The `embeddings` field carries the embeddings produced during the scoring
/// pass.  The storage/ingest layer **MUST NOT** call `embed_batch` on the
/// winner again — it should reuse `embeddings` directly.
///
/// <!-- SINGLE-PASS: embeddings reused from scoring pass, do NOT call embed_batch on winner again -->
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    /// Chunk drafts produced by the winning generator.
    pub drafts: Vec<ChunkDraft>,
    /// Chunk-level embeddings in the same order as `drafts`.
    ///
    /// SINGLE-PASS: embeddings reused from scoring pass, do NOT call
    /// embed_batch on winner again.
    pub embeddings: Vec<Vec<f32>>,
    /// Heading records from the **structural** parse (used for DocTree, not
    /// tied to which candidate wins).
    pub heading_records: Vec<HeadingRecord>,
    /// Name of the winning generator (from [`CandidateGenerator::name`]).
    pub winner_name: String,
}

/// Context passed to every [`CandidateGenerator`] and metric.
#[derive(Clone)]
pub struct GeneratorContext {
    /// Target chunk size in tokens.
    pub target_tokens: usize,
    /// Token overlap between adjacent chunks.
    pub overlap_tokens: usize,
    /// Pre-computed unit embeddings for the source text.
    ///
    /// Produced once in `run_bakeoff` by embedding all `TextUnit`s.  Generators
    /// that need semantic similarity (e.g. [`SemanticBreakpointGenerator`])
    /// consume these rather than calling the embedder themselves.
    pub unit_embeddings: Vec<Vec<f32>>,
}

/// A generator that produces one candidate chunk list from a source document.
///
/// # Object safety
///
/// The trait is **sync** (no `async fn`) to be dyn-compatible on MSRV 1.94.
/// Generators that need embeddings receive them via [`GeneratorContext`].
///
/// # Error handling
///
/// Returning `Err(e)` causes the bake-off to **drop this generator** (logged
/// via `tracing::warn!`) without aborting ingest.  The structural generator
/// MUST never return `Err` — it is the guaranteed fallback floor.
pub trait CandidateGenerator: Send + Sync {
    /// Generate a candidate chunk list for `source_text`.
    ///
    /// Returns `Err` only for truly unrecoverable generator failures; returning
    /// `Err` causes the bake-off to silently skip this candidate.
    fn generate(
        &self,
        source_text: &str,
        ctx: &GeneratorContext,
        counter: &dyn TokenCounter,
    ) -> Result<Vec<ChunkDraft>, LunarisError>;

    /// Human-readable name used in tracing spans and warning messages.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// StructuralGenerator — the always-present fallback (wraps the existing chunker)
// ---------------------------------------------------------------------------

/// Wraps [`chunk_markdown_with_headings_with_counter`] as a [`CandidateGenerator`].
///
/// This generator is the **guaranteed floor**: it never returns `Err`, and the
/// bake-off ensures it is always included regardless of errors from other generators.
#[derive(Debug, Clone, Default)]
pub struct StructuralGenerator;

impl CandidateGenerator for StructuralGenerator {
    fn generate(
        &self,
        source_text: &str,
        ctx: &GeneratorContext,
        counter: &dyn TokenCounter,
    ) -> Result<Vec<ChunkDraft>, LunarisError> {
        let (drafts, _heading_records) = chunk_markdown_with_headings_with_counter(
            source_text,
            ctx.target_tokens,
            ctx.overlap_tokens,
            counter,
        );
        Ok(drafts)
    }

    fn name(&self) -> &'static str {
        "structural"
    }
}

// ---------------------------------------------------------------------------
// SemanticBreakpointGenerator — split at cosine local-minima of unit embeddings
// ---------------------------------------------------------------------------

/// Splits the source into chunks by finding local minima in the cosine
/// similarity between adjacent unit embeddings.
///
/// A boundary is placed between unit[i] and unit[i+1] when
/// `cosine(unit[i], unit[i+1]) < threshold`, indicating a semantic shift.
#[derive(Debug, Clone)]
pub struct SemanticBreakpointGenerator {
    /// Cosine similarity threshold below which a boundary is placed.
    pub threshold: f32,
}

impl Default for SemanticBreakpointGenerator {
    fn default() -> Self {
        Self { threshold: 0.5 }
    }
}

impl CandidateGenerator for SemanticBreakpointGenerator {
    fn generate(
        &self,
        source_text: &str,
        ctx: &GeneratorContext,
        counter: &dyn TokenCounter,
    ) -> Result<Vec<ChunkDraft>, LunarisError> {
        let units = segment_units(source_text, SegmentMode::Sentence);
        if units.is_empty() {
            return Ok(Vec::new());
        }

        // Place boundaries where adjacent cosine < threshold (local minimum)
        let mut boundaries: Vec<usize> = Vec::new(); // unit indices starting a new chunk
        boundaries.push(0);

        let embs = &ctx.unit_embeddings;
        if embs.len() >= units.len() {
            for i in 0..units.len().saturating_sub(1) {
                let sim = cosine_f32(&embs[i], &embs[i + 1]);
                if sim < self.threshold {
                    boundaries.push(i + 1);
                }
            }
        }
        // Always end after last unit
        boundaries.push(units.len());

        let mut drafts = Vec::new();
        let mut offset: u32 = 0;
        for window in boundaries.windows(2) {
            let start = window[0];
            let end = window[1];
            if start >= end {
                continue;
            }
            let text =
                units[start..end].iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join(" ");
            if text.trim().is_empty() {
                continue;
            }
            // Use the caller-supplied counter (BPE or surrogate) for
            // SizeCompliance scoring — the surrogate must NOT be hardcoded here
            // as that would make bake-off comparisons unfair (F2 fix).
            let trimmed = text.trim().to_string();
            let tokens = counter.count(&trimmed);
            // Best-effort byte span: find the first word of the chunk in the
            // source to anchor the start, then advance by the chunk text length.
            // This is an approximation (whitespace normalisation means
            // `source_text.find(trimmed)` would fail for joined units), but it
            // is sufficient for BlockIntegrityMetric to distinguish boundary
            // alignment vs mid-block splits in the bake-off.
            // TODO(phase-29): replace with precise unit-level byte offsets once
            // segment.rs upgrades to into_offset_iter() for char_offset accuracy.
            let source_byte_span = first_word_span(&trimmed, source_text);
            drafts.push(ChunkDraft {
                text: trimmed,
                heading_path: Vec::new(),
                offset,
                tokens,
                overlap_tail: String::new(),
                source_byte_span,
            });
            offset += 1;
        }
        Ok(drafts)
    }

    fn name(&self) -> &'static str {
        "semantic-breakpoint"
    }
}

// ---------------------------------------------------------------------------
// SizeVariantGenerator — structural at a scaled target
// ---------------------------------------------------------------------------

/// Wraps the structural generator with a scaled `target_tokens`.
///
/// A multiplier > 1.0 produces coarser (fewer, larger) chunks.
/// A multiplier < 1.0 produces finer (more, smaller) chunks.
#[derive(Debug, Clone)]
pub struct SizeVariantGenerator {
    /// Scale factor applied to `target_tokens`.
    pub multiplier: f32,
}

impl Default for SizeVariantGenerator {
    fn default() -> Self {
        Self { multiplier: 2.0 }
    }
}

impl CandidateGenerator for SizeVariantGenerator {
    fn generate(
        &self,
        source_text: &str,
        ctx: &GeneratorContext,
        counter: &dyn TokenCounter,
    ) -> Result<Vec<ChunkDraft>, LunarisError> {
        let scaled_target = ((ctx.target_tokens as f32 * self.multiplier) as usize).max(1);
        let (drafts, _) = chunk_markdown_with_headings_with_counter(
            source_text,
            scaled_target,
            ctx.overlap_tokens,
            counter,
        );
        Ok(drafts)
    }

    fn name(&self) -> &'static str {
        "size-variant"
    }
}

// ---------------------------------------------------------------------------
// RecursiveSplitMergeGenerator
// ---------------------------------------------------------------------------

/// Recursively splits units that exceed `target_tokens` and merges consecutive
/// undersized units to approach the target.
#[derive(Debug, Clone, Default)]
pub struct RecursiveSplitMergeGenerator;

impl CandidateGenerator for RecursiveSplitMergeGenerator {
    fn generate(
        &self,
        source_text: &str,
        ctx: &GeneratorContext,
        counter: &dyn TokenCounter,
    ) -> Result<Vec<ChunkDraft>, LunarisError> {
        let units = segment_units(source_text, SegmentMode::Sentence);
        if units.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 1: split oversized units by bisection
        let mut pieces: Vec<String> = Vec::new();
        for unit in &units {
            let tok = counter.count(&unit.text) as usize;
            if tok > ctx.target_tokens {
                split_into_pieces(&unit.text, ctx.target_tokens, counter, &mut pieces);
            } else {
                pieces.push(unit.text.clone());
            }
        }

        // Phase 2: merge undersized consecutive pieces toward target
        let mut merged: Vec<String> = Vec::new();
        let mut acc = String::new();
        for piece in pieces {
            if acc.is_empty() {
                acc = piece;
            } else {
                let candidate = format!("{acc} {piece}");
                if counter.count(&candidate) as usize <= ctx.target_tokens {
                    acc = candidate;
                } else {
                    merged.push(acc);
                    acc = piece;
                }
            }
        }
        if !acc.trim().is_empty() {
            merged.push(acc);
        }

        let drafts: Vec<ChunkDraft> = merged
            .into_iter()
            .enumerate()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(i, text)| {
                let trimmed = text.trim().to_string();
                let tokens = counter.count(&trimmed);
                // Best-effort byte span — same approximation as
                // SemanticBreakpointGenerator (see comment there).
                // TODO(phase-29): replace with precise unit-level byte offsets.
                let source_byte_span = first_word_span(&trimmed, source_text);
                ChunkDraft {
                    text: trimmed,
                    heading_path: Vec::new(),
                    offset: i as u32,
                    tokens,
                    overlap_tail: String::new(),
                    source_byte_span,
                }
            })
            .collect();
        Ok(drafts)
    }

    fn name(&self) -> &'static str {
        "recursive-split-merge"
    }
}

fn split_into_pieces(text: &str, target: usize, counter: &dyn TokenCounter, out: &mut Vec<String>) {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return;
    }
    let mid = words.len() / 2;
    let left = words[..mid].join(" ");
    let right = words[mid..].join(" ");
    if counter.count(&left) as usize > target && mid > 1 {
        split_into_pieces(&left, target, counter, out);
    } else if !left.trim().is_empty() {
        out.push(left);
    }
    if counter.count(&right) as usize > target && (words.len() - mid) > 1 {
        split_into_pieces(&right, target, counter, out);
    } else if !right.trim().is_empty() {
        out.push(right);
    }
}

// ---------------------------------------------------------------------------
// Bakeoff configuration
// ---------------------------------------------------------------------------

/// Configuration for the adaptive meta-framework bake-off.
pub struct BakoffConfig {
    /// Selector weights (default: SC=0.15/ICC=0.30/DCC=0.20/BI=0.20/RC=0.15).
    pub weights: crate::chunker::selector::SelectorWeights,
    /// Maximum number of candidates to evaluate (default 3).
    pub max_candidates: usize,
    /// Generators to run. The bake-off always includes [`StructuralGenerator`];
    /// any generator here that errors is silently dropped.
    pub generators: Vec<Box<dyn CandidateGenerator + Send + Sync>>,
    /// Target chunk size in tokens. Passed to every generator and the SC metric.
    /// Default: 500 (mirrors `DEFAULT_TARGET_TOKENS` in `lunaris-ingest::pipeline`).
    pub target_tokens: usize,
    /// Overlap in tokens between consecutive chunks. Default: 100.
    pub overlap_tokens: usize,
}

impl std::fmt::Debug for BakoffConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BakoffConfig")
            .field("max_candidates", &self.max_candidates)
            .field("generator_count", &self.generators.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// run_bakeoff
// ---------------------------------------------------------------------------

/// Run the bake-off: generate candidates → embed once → score → select winner.
///
/// # Parameters
///
/// - `target_tokens` / `overlap_tokens`: govern every generator and the SC
///   metric.  Pass the same values used by the calling ingest pipeline so that
///   the winning candidate is scored against the correct target size.
///
/// # Resilience
///
/// Generators that return `Err` are dropped with `tracing::warn!`.  The
/// structural generator is always present as the fallback floor.
///
/// # SINGLE-PASS
///
/// For each candidate, `embed_batch` is called **once** on all chunk texts.
/// The resulting vectors are stored in [`ScoredCandidate::embeddings`] and
/// reused by both the metrics and the storage layer — the winner is never
/// re-embedded.
/// Run the bake-off: generate candidates → embed once → score → select winner.
///
/// # Errors
///
/// Returns `Err` when the embedder fails or returns the wrong row count.
/// This is a hard infrastructure failure — the bake-off cannot score or store
/// chunks without valid embeddings. Fail loud rather than silently storing
/// all-zero vectors that would make episodes invisible to vector recall.
///
/// Graceful degradation / budget-governor fallback is Phase 31 GOV-01;
/// it is NOT implemented here. The bake-off is opt-in; an embedder failure
/// in the bake-off path is a real infrastructure failure that must surface.
pub async fn run_bakeoff(
    source_text: &str,
    structural_heading_records: Vec<HeadingRecord>,
    config: &BakoffConfig,
    embedder: &dyn Embedder,
    counter: &dyn TokenCounter,
    target_tokens: usize,
    overlap_tokens: usize,
) -> Result<ScoredCandidate, LunarisError> {
    use crate::chunker::metrics::MetricContext;
    use crate::chunker::selector::ChunkSelector;

    // Step 1: embed unit texts once for generators that need semantic context.
    // Fail loud: a wrong count or error here corrupts the semantic-breakpoint
    // generator's similarity computations. Do NOT zero-fill.
    let units = segment_units(source_text, SegmentMode::Sentence);
    let unit_texts: Vec<&str> = units.iter().map(|u| u.text.as_str()).collect();
    let unit_embeddings = match embedder.embed_batch(&unit_texts).await {
        Ok(v) if v.len() == unit_texts.len() => v,
        Ok(got) => {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "run_bakeoff: unit embed_batch returned {} rows, expected {}; \
                 refusing to zero-fill (would corrupt vector storage)",
                got.len(),
                unit_texts.len()
            ))));
        }
        Err(e) => {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "run_bakeoff: unit embed_batch failed: {e}; \
                 refusing to zero-fill (would corrupt vector storage)"
            ))));
        }
    };

    let gen_ctx = GeneratorContext { target_tokens, overlap_tokens, unit_embeddings };

    // Step 2: collect structural candidate first (the fallback floor, never drops)
    let structural_gen = StructuralGenerator;
    let structural_drafts = structural_gen
        .generate(source_text, &gen_ctx, counter)
        .expect("StructuralGenerator must never fail");

    // Step 3: collect up to max_candidates - 1 additional candidates.
    // Track (generator_name, drafts) so we can surface the winner's name.
    let extra_cap = config.max_candidates.saturating_sub(1);
    let mut all_named: Vec<(&'static str, Vec<ChunkDraft>)> =
        Vec::with_capacity(config.max_candidates);
    all_named.push((structural_gen.name(), structural_drafts));

    for generator in config.generators.iter().take(extra_cap) {
        match generator.generate(source_text, &gen_ctx, counter) {
            Ok(drafts) if !drafts.is_empty() => all_named.push((generator.name(), drafts)),
            Ok(_) => {
                tracing::warn!(
                    generator = generator.name(),
                    "generator produced no chunks; dropping"
                );
            }
            Err(e) => {
                tracing::warn!(
                    generator = generator.name(),
                    err = %e,
                    "generator errored; dropping from bake-off"
                );
            }
        }
    }

    // Step 4: embed chunk texts once per candidate (SINGLE-PASS)
    // ICC/DCC/storage all use these vectors; no re-embedding after this point.
    let mut names: Vec<&'static str> = Vec::with_capacity(all_named.len());
    let mut scored: Vec<crate::chunker::metrics::CandidateWithEmbeddings> =
        Vec::with_capacity(all_named.len());

    for (name, drafts) in all_named {
        let chunk_texts: Vec<&str> = drafts.iter().map(|d| d.text.as_str()).collect();
        // Fail loud: do NOT zero-fill on embed_batch failure or wrong row count.
        // Silently storing all-zero vectors would make the episode invisible to
        // vector recall with only a warn log — a silent storage corruption.
        // Phase 31 GOV-01 will add graceful degradation; until then, fail hard.
        let chunk_embs = match embedder.embed_batch(&chunk_texts).await {
            Ok(v) if v.len() == chunk_texts.len() => v,
            Ok(got) => {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "run_bakeoff: chunk embed_batch for candidate '{}' returned {} rows, \
                     expected {}; refusing to zero-fill (would corrupt vector storage)",
                    name,
                    got.len(),
                    chunk_texts.len()
                ))));
            }
            Err(e) => {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "run_bakeoff: chunk embed_batch for candidate '{}' failed: {e}; \
                     refusing to zero-fill (would corrupt vector storage)",
                    name
                ))));
            }
        };
        names.push(name);
        scored.push(crate::chunker::metrics::CandidateWithEmbeddings {
            drafts,
            chunk_embeddings: chunk_embs,
        });
    }

    // Step 5: score candidates and select winner
    // TODO(phase-future): pass entity annotations here once two-pass ingest is
    // implemented (extraction must precede bake-off scoring for entity-aware RC).
    // For now, production always passes None — chunking precedes extraction so
    // entity spans are unavailable at bake-off time. See MetricContext::entities
    // doc comment for the full rationale.
    let metric_ctx =
        MetricContext { target_tokens: gen_ctx.target_tokens as u32, source_text, entities: None };

    let winner_idx = ChunkSelector::select_with_embeddings(&scored, &metric_ctx, &config.weights);
    let winner_name = names[winner_idx].to_string();
    let winner = scored.swap_remove(winner_idx);

    Ok(ScoredCandidate {
        drafts: winner.drafts,
        embeddings: winner.chunk_embeddings,
        heading_records: structural_heading_records,
        winner_name,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort byte span for a non-structural chunk in its original source.
///
/// Finds the first whitespace-delimited word of `chunk_text` in `source` and
/// uses that as the span start; span end = start + `chunk_text.len()`.  This
/// approximation works when the chunk's leading word is a verbatim substring of
/// the source, which is true for normalised sentence/paragraph joins where only
/// internal whitespace differs.
///
/// Returns `None` when the first word cannot be found (empty chunk, empty source,
/// or pathological cases).
///
/// # Limitations
///
/// - If the first word appears multiple times in the source before the actual
///   chunk position, the span may anchor to the wrong occurrence.  For bake-off
///   BI metric purposes this is acceptable — false positives (correct score) are
///   more likely than false negatives.
/// - TODO(phase-29): replace with precise unit-level byte offsets once
///   `segment.rs` upgrades to `into_offset_iter()`.
fn first_word_span(chunk_text: &str, source: &str) -> Option<(usize, usize)> {
    let first_word = chunk_text.split_whitespace().next()?;
    if first_word.is_empty() || source.is_empty() {
        return None;
    }
    let start = source.find(first_word)?;
    let end = (start + chunk_text.len()).min(source.len());
    Some((start, end))
}

/// Compute the cosine similarity between two f32 vectors.
/// Returns 0.0 when either vector is zero-length or all-zeros.
pub fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < 1e-12 || nb < 1e-12 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

// ---------------------------------------------------------------------------
// Tests (T-28-01 red → green)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::SurrogateTokenCounter;
    use lunaris_core::StubEmbedder;

    // ── Trait object safety ──────────────────────────────────────────────────

    /// Compile-time check: `Box<dyn CandidateGenerator>` is constructible.
    #[test]
    fn candidate_generator_trait_is_object_safe() {
        struct NoopGen;
        impl CandidateGenerator for NoopGen {
            fn generate(
                &self,
                _src: &str,
                _ctx: &GeneratorContext,
                _counter: &dyn TokenCounter,
            ) -> Result<Vec<ChunkDraft>, LunarisError> {
                Ok(Vec::new())
            }
            fn name(&self) -> &'static str {
                "noop"
            }
        }
        let _: Box<dyn CandidateGenerator> = Box::new(NoopGen);
    }

    // ── StructuralGenerator never drops ─────────────────────────────────────

    fn stub_ctx(unit_embeddings: Vec<Vec<f32>>) -> GeneratorContext {
        GeneratorContext { target_tokens: 500, overlap_tokens: 100, unit_embeddings }
    }

    fn surrogate() -> SurrogateTokenCounter {
        SurrogateTokenCounter
    }

    #[test]
    fn structural_generator_never_drops_on_empty() {
        let sgen = StructuralGenerator;
        let ctx = stub_ctx(Vec::new());
        let result = sgen.generate("", &ctx, &surrogate());
        assert!(result.is_ok(), "structural must not Err on empty input");
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn structural_generator_never_drops_on_heading_only() {
        let sgen = StructuralGenerator;
        let ctx = stub_ctx(Vec::new());
        let result = sgen.generate("# Just a heading\n\n## Sub heading", &ctx, &surrogate());
        assert!(result.is_ok(), "structural must not Err on heading-only input");
    }

    #[test]
    fn structural_generator_never_drops_on_oversized_block() {
        let sgen = StructuralGenerator;
        let ctx = stub_ctx(Vec::new());
        // Generate a very large block (10 000 words) — should still succeed
        let big_text = "word ".repeat(10_000);
        let result = sgen.generate(&big_text, &ctx, &surrogate());
        assert!(result.is_ok(), "structural must not Err on oversized block");
        assert!(!result.unwrap().is_empty(), "oversized block must produce at least 1 chunk");
    }

    #[test]
    fn structural_generator_never_drops_on_valid_markdown() {
        let sgen = StructuralGenerator;
        let ctx = stub_ctx(Vec::new());
        let md = "# Title\n\nFirst paragraph text here.\n\n## Section\n\nMore text.";
        let result = sgen.generate(md, &ctx, &surrogate());
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }

    // ── ScoredCandidate carries embeddings ──────────────────────────────────

    #[test]
    fn scored_candidate_holds_embeddings() {
        let sc = ScoredCandidate {
            drafts: Vec::new(),
            embeddings: vec![vec![1.0, 0.0]],
            heading_records: Vec::new(),
            winner_name: "structural".to_string(),
        };
        assert_eq!(sc.embeddings.len(), 1);
    }

    // ── cosine_f32 sanity ────────────────────────────────────────────────────

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_f32(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine_f32(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_empty_input_is_zero() {
        assert_eq!(cosine_f32(&[], &[1.0]), 0.0);
        assert_eq!(cosine_f32(&[], &[]), 0.0);
    }

    // ── StubEmbedder returns distinct vectors for distinct inputs ───────────

    #[tokio::test]
    async fn stub_embedder_is_deterministic() {
        let emb = StubEmbedder::new(4);
        let a = emb.embed_batch(&["hello"]).await.unwrap();
        let b = emb.embed_batch(&["hello"]).await.unwrap();
        assert_eq!(a, b);
    }

    // ── F1: embed_batch failure must propagate as Err, never zero-fill ───────

    /// RED test (F1-a): when embed_batch errors during the chunk-embed pass,
    /// run_bakeoff must return Err — NOT silently store all-zero vectors.
    #[tokio::test]
    async fn f1_chunk_embed_failure_propagates_err() {
        use crate::chunker::selector::SelectorWeights;
        use crate::chunker::{
            BakoffConfig, SurrogateTokenCounter, chunk_markdown_with_headings_with_counter,
            run_bakeoff,
        };
        use lunaris_core::FailingEmbedder;

        let embedder = FailingEmbedder::new(4);
        let counter = SurrogateTokenCounter;
        let doc = "First paragraph.\n\nSecond paragraph with more words.";
        let (_, heading_records) =
            chunk_markdown_with_headings_with_counter(doc, 500, 100, &counter);

        let config = BakoffConfig {
            weights: SelectorWeights::default(),
            max_candidates: 1,
            generators: vec![],
            target_tokens: 500,
            overlap_tokens: 100,
        };

        let result =
            run_bakeoff(doc, heading_records, &config, &embedder, &counter, 500, 100).await;
        assert!(result.is_err(), "embed_batch failure must propagate as Err, not produce a winner");
        // Extra guard: ensure no all-zero vector could have been silently produced.
        // (If result is Ok, that would be the corruption — already caught above.)
    }

    /// RED test (F1-b): when embed_batch returns wrong row count during the
    /// chunk-embed pass, run_bakeoff must return Err.
    #[tokio::test]
    async fn f1_chunk_embed_wrong_row_count_propagates_err() {
        use crate::chunker::selector::SelectorWeights;
        use crate::chunker::{
            BakoffConfig, SurrogateTokenCounter, chunk_markdown_with_headings_with_counter,
            run_bakeoff,
        };
        use lunaris_core::FailingEmbedder;

        let embedder = FailingEmbedder::wrong_count(4);
        let counter = SurrogateTokenCounter;
        let doc = "First paragraph.\n\nSecond paragraph with more words.";
        let (_, heading_records) =
            chunk_markdown_with_headings_with_counter(doc, 500, 100, &counter);

        let config = BakoffConfig {
            weights: SelectorWeights::default(),
            max_candidates: 1,
            generators: vec![],
            target_tokens: 500,
            overlap_tokens: 100,
        };

        let result =
            run_bakeoff(doc, heading_records, &config, &embedder, &counter, 500, 100).await;
        assert!(
            result.is_err(),
            "wrong row count must propagate as Err, not produce a winner with zero-fill vectors"
        );
    }

    // ── F2: SemanticBreakpointGenerator must use the injected TokenCounter ────

    /// A counter whose output differs systematically from `SurrogateTokenCounter`:
    /// always returns `tokens = 1` regardless of text length.  If
    /// `SemanticBreakpointGenerator` still uses the internal
    /// `SurrogateTokenCounter`, the token counts on produced `ChunkDraft`s will
    /// reflect the surrogate (word-count × 1.3) instead of 1.
    struct ConstOneCounter;
    impl crate::chunker::TokenCounter for ConstOneCounter {
        fn count(&self, _text: &str) -> u32 {
            1
        }
    }

    /// RED test (F2): `SemanticBreakpointGenerator` must honour the counter
    /// passed in via the trait parameter. With `ConstOneCounter`, every produced
    /// draft must have `tokens == 1`. If the generator hardcodes
    /// `SurrogateTokenCounter`, the token counts will differ.
    #[test]
    fn f2_semantic_breakpoint_uses_injected_counter() {
        let sbgen = SemanticBreakpointGenerator::default();
        // Provide non-trivial unit embeddings so the generator produces at
        // least one chunk — all-zeros would give sim=0 < 0.5 everywhere and
        // might collapse to empty.
        let unit_embeddings =
            vec![vec![1.0f32, 0.0, 0.0], vec![1.0f32, 0.0, 0.0], vec![1.0f32, 0.0, 0.0]];
        let ctx = GeneratorContext { target_tokens: 500, overlap_tokens: 0, unit_embeddings };
        let text = "Alpha beta gamma delta epsilon. Zeta eta theta. Iota kappa lambda.";
        let counter = ConstOneCounter;
        let drafts = sbgen.generate(text, &ctx, &counter).unwrap();
        assert!(
            !drafts.is_empty(),
            "SemanticBreakpointGenerator must produce at least one chunk on non-empty input"
        );
        for draft in &drafts {
            assert_eq!(
                draft.tokens, 1,
                "SemanticBreakpointGenerator must use the injected counter (ConstOneCounter \
                 returns 1 for every text); got tokens={} for chunk {:?}",
                draft.tokens, draft.text
            );
        }
    }
}
