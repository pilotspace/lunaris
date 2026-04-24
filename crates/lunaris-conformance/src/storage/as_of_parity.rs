//! Plan 05-02 STORE-07 — AS_OF query semantics identical across both
//! backends.
//!
//! Ingests the same 10-episode [`FixtureCorpus`] into Moon and Postgres,
//! then for each `(query, as_of)` tuple in the fixture's query set runs
//! the same `vector_search` against both backends and asserts the hit
//! sets match field-by-field. Divergences are accumulated into a typed
//! [`Divergence`] vec so the failure mode lists EVERY mismatch instead
//! of stopping at the first one (T-05-02-04 mitigation).
//!
//! Per CONTEXT.md D-12 the score-epsilon comparison is gated on
//! [`StorageCapabilities`] — when one backend reports
//! `rerank_native = false` (Postgres) and the other reports `true`
//! (Moon), score precision divergence is documented in the
//! [`DivergenceKind::ScoreEpsilon`] variant rather than blanket failure.
//! The hit-id ordering invariant (`hits_moon[i].id == hits_pg[i].id`)
//! still applies regardless of capability skew.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::hlc::Hlc;
use lunaris_core::storage::StoragePort;

use crate::fixtures::{stub_embed, FixtureCorpus};

/// One observed divergence between the two backends for a given
/// `(query, as_of)` tuple. Surfaced verbatim in the `anyhow::bail!`
/// message when the parity test fails (T-05-02-04 — divergences are
/// never silently swallowed).
#[derive(Debug)]
pub struct Divergence {
    pub query: String,
    pub as_of: Option<Hlc>,
    pub kind: DivergenceKind,
}

#[derive(Debug)]
pub enum DivergenceKind {
    /// Backends returned a different number of hits.
    HitCount { moon: usize, postgres: usize },
    /// Backends agreed on hit count but disagreed on the id at this
    /// position (zero-indexed).
    HitOrdering {
        position: usize,
        moon_id: Vec<u8>,
        postgres_id: Vec<u8>,
    },
    /// Backends agreed on id ordering but the score difference at this
    /// position exceeded `epsilon`. Only fired when both backends report
    /// the same `rerank_native` capability — when capabilities differ,
    /// score divergence is expected and gated out.
    ScoreEpsilon {
        position: usize,
        moon_score: f32,
        postgres_score: f32,
        epsilon: f32,
    },
}

/// Score-comparison tolerance when both backends advertise the same
/// rerank capability. Tuned at 0.001 — tighter than typical `f32`
/// rounding noise across two independent SIMD pipelines, looser than a
/// strict bit-exact compare which would never hold across CPU
/// vendors.
const SCORE_EPSILON: f32 = 0.001;

/// Run the AS_OF parity test against both backends.
///
/// Ingests the deterministic fixture corpus into both, then walks the
/// query set and accumulates every observed [`Divergence`]. Returns
/// `Err` listing all divergences if any are found; `Ok(())` otherwise.
pub async fn run(
    moon: &Arc<dyn StoragePort>,
    postgres: &Arc<dyn StoragePort>,
    fixtures: &FixtureCorpus,
) -> anyhow::Result<()> {
    fixtures.ingest_into(moon).await?;
    fixtures.ingest_into(postgres).await?;

    let moon_caps = moon.capabilities();
    let pg_caps = postgres.capabilities();
    let compare_scores = moon_caps.rerank_native == pg_caps.rerank_native;

    let mut divergences: Vec<Divergence> = Vec::new();

    for (query, as_of) in fixtures.query_set() {
        let q_vec = stub_embed(query);

        let hits_moon = moon
            .vector_search("chunks", &q_vec, 5, None, *as_of, false)
            .await?;
        let hits_pg = postgres
            .vector_search("chunks", &q_vec, 5, None, *as_of, false)
            .await?;

        if hits_moon.len() != hits_pg.len() {
            divergences.push(Divergence {
                query: query.clone(),
                as_of: *as_of,
                kind: DivergenceKind::HitCount {
                    moon: hits_moon.len(),
                    postgres: hits_pg.len(),
                },
            });
            // Ordering comparison is meaningless when counts differ;
            // skip to next query.
            continue;
        }

        for (i, (m, p)) in hits_moon.iter().zip(hits_pg.iter()).enumerate() {
            if m.id != p.id {
                divergences.push(Divergence {
                    query: query.clone(),
                    as_of: *as_of,
                    kind: DivergenceKind::HitOrdering {
                        position: i,
                        moon_id: m.id.clone(),
                        postgres_id: p.id.clone(),
                    },
                });
                // First ordering mismatch invalidates remaining
                // positional asserts for this query — break inner.
                break;
            }
            if compare_scores && (m.score - p.score).abs() > SCORE_EPSILON {
                divergences.push(Divergence {
                    query: query.clone(),
                    as_of: *as_of,
                    kind: DivergenceKind::ScoreEpsilon {
                        position: i,
                        moon_score: m.score,
                        postgres_score: p.score,
                        epsilon: SCORE_EPSILON,
                    },
                });
                break;
            }
        }
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "STORE-07 AS_OF parity violations (moon_caps={moon_caps:?}, pg_caps={pg_caps:?}): {divergences:#?}"
        );
    }
    Ok(())
}

// `stub_embed` moved to `crate::fixtures` so the ingest path and the query
// path compute matching 768d vectors for the same input string. The former
// 16d local copy caused a dimension-mismatch panic against Moon's 768d FT
// index AND against Postgres's `vector(768)` column (debug session
// `.planning/debug/conformance-dim-mismatch.md`, 2026-04-24).
