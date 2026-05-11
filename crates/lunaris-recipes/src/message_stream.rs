//! Phase 9 Plan 09-01 PRIM-01 — `MessageStream` primitive.
//!
//! Recency-weighted message recall for conversational recipes (Phase 10
//! ChatAgentMemory / MultiTurnConversation / SlackArchive / EmailThreading /
//! MeetingNotesMemory all wrap this primitive — see ROADMAP Phase 10).
//!
//! ## ACT-R base-level activation (blueprint §5, Anderson 1996)
//!
//! `B(i) = ln(sum_k t_k^-d)` with `d = 0.5` per Anderson (1996), where `t_k`
//! is the age (seconds) of the k-th prior access of item `i`. More recent
//! accesses raise the score; the logarithm tempers the curve. This is the
//! SAME formula used by `lunaris_consolidate::ActRScorer` for PROMOTION —
//! recipes apply it to RETRIEVAL rerank, not promotion.
//!
//! ## ≤ 30 LOC public-surface contract (PRIM-01)
//!
//! Public symbols on this module: [`MessageStream::new`], [`MessageStream::ingest`],
//! [`MessageStream::recall`], and [`MessageStream::with_top_k`] (builder-style
//! testability knob). Four `pub fn` / `pub async fn` declarations — the
//! LOC-guard test at the bottom of this file counts them and caps at 6.
//!
//! ## Plan-vs-code corrections applied (Rule 1 deviations documented in 09-01-SUMMARY.md)
//!
//! The plan text assumed several APIs that don't exist on the v0.1.0 surface:
//! `WriteOp::upsert_episode` (fictional), `Hlc::now()` (only `HlcClock::tick()`
//! exists), and `Hit::valid_time: Option<Hlc>` (real shape is `valid_from: Hlc`).
//! The implementation below matches the actual surface — `ingest` takes ONE
//! message and delegates to `Lunaris::ingest(Episode)` (which goes through the
//! full chunker + embedder + atomic_write fan-out, so `recall` actually finds
//! the message via Vector + Keyword), and the ACT-R scorer reads
//! `hit.valid_from` directly. "Single atomic_write per ingest call" is
//! trivially true because one call = one message = one internal atomic_write
//! inside `lunaris_ingest::ingest_episode` (INGEST-04 contract preserved by
//! the umbrella — this primitive does not hand-build WriteOps).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::storage::types::Lsn;
use lunaris_core::{BiTemporal, Episode, Hlc, LunarisError};
use lunaris_retrieve::{Hit, Keyword, Query, Vector};
use ulid::Ulid;

/// Default `top_k` the [`MessageStream::recall`] path uses when the caller does
/// not override via [`MessageStream::with_top_k`]. Chosen to match the Phase 5
/// `HeliosScratchpad::read` default (8) so conversational wrappers in Phase 10
/// inherit the same breadth.
const DEFAULT_TOP_K: usize = 8;

/// ACT-R decay constant `d` per Anderson (1996) — base-level activation
/// `B(i) = ln(sum_k t_k^-d)`. Do NOT tune without also updating the unit test
/// `message_stream_recency_scorer_matches_act_r_formula` below.
const ACT_R_DECAY: f32 = 0.5;

/// Minimum age clamp (seconds). Prevents `t^-0.5` from blowing up when a hit's
/// `valid_from` is the same HLC wall-millis as `now` (common in tight
/// ingest-then-recall test loops).
const MIN_AGE_SECONDS: f32 = 1.0;

/// Message-stream primitive. Wraps [`Arc<Lunaris>`] and exposes a
/// recency-weighted recall path for conversational sources.
///
/// State is immutable after construction (`lunaris` is an `Arc`; `thread_prefix`
/// is an owned `String`; `top_k` is `Copy`), so this type holds ZERO
/// `RwLock`s — CLAUDE.md lock discipline is satisfied trivially (no guard
/// can be held across `.await` because there are no guards).
#[derive(Clone)]
pub struct MessageStream {
    lunaris: Arc<Lunaris>,
    thread_prefix: String,
    top_k: usize,
}

impl MessageStream {
    /// Construct a new [`MessageStream`] bound to `thread_prefix`. Every
    /// [`ingest`](Self::ingest) / [`recall`](Self::recall) call scopes
    /// through this prefix via [`Filter::StartsWith`] on the Episode `source`
    /// field. Conventional value: `"messages:"`; per-wrapper recipes
    /// specialise further (e.g. `"slack:archive/"`, `"email:"`).
    pub fn new(lunaris: Arc<Lunaris>, thread_prefix: impl Into<String>) -> Self {
        Self { lunaris, thread_prefix: thread_prefix.into(), top_k: DEFAULT_TOP_K }
    }

    /// Override the default `top_k` for [`recall`](Self::recall). Returns
    /// `self` for builder-style chaining.
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    /// Ingest one message as an [`Episode`] under
    /// `{thread_prefix}{thread_id}/`. Delegates to [`Lunaris::ingest`] — the
    /// umbrella pipeline performs chunking + embedding + a SINGLE
    /// `atomic_write` fan-out (INGEST-04 contract). `thread_id` +
    /// `participant_id` land in the Episode `metadata` map so Wave 2 / Phase 10
    /// wrappers can filter on them.
    pub async fn ingest(
        &self,
        message: impl Into<String>,
        thread_id: impl Into<String>,
        participant_id: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        let thread_id = thread_id.into();
        let participant_id = participant_id.into();
        let mut metadata = serde_json::Map::new();
        metadata.insert("thread_id".into(), serde_json::Value::String(thread_id.clone()));
        metadata.insert("participant_id".into(), serde_json::Value::String(participant_id));
        let episode = Episode {
            id: Ulid::new(),
            // RFC 0001 Wave 1D: use Scope::dev() until MessageStream receives
            // a real Scope (Wave 3 SDK layer). The scope is embedded in the
            // `source` prefix for now.
            scope: lunaris_core::Scope::dev(),
            source: format!("{}{}/", self.thread_prefix, thread_id),
            content: message.into(),
            t_ref: None,
            // Server stamps `bt` at ingest time via the chunker's
            // `BiTemporal::now(clock)` per Plan 02-01 — matches the
            // helios_scratchpad.rs:103 convention.
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            metadata,
        };
        self.lunaris.ingest(episode).await
    }

    /// Recall with recency-weighted ordering. Fuses `Vector + Keyword` via
    /// RRF(k=60), filters to `thread_prefix` via [`Filter::StartsWith`], then
    /// blends the ACT-R base-level activation score (Anderson 1996,
    /// `d = 0.5`) with the fused RRF score to rerank hits by age.
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        // Source-prefix scoping runs post-hydrate (not via StoragePort filter).
        // The Moon `chunks` FT schema does not carry `source`, and Moon's TAG
        // index is exact-match only (store.rs::search_tag, verified
        // 2026-04-23) so a prefix push-down is infeasible on that backend.
        // `Hit.source` is populated by `lunaris_retrieve::hydrate` — we
        // over-fetch and prune here for parity with Postgres.
        let over_fetch = self.top_k * 12;
        let plan = Vector::new("chunks", over_fetch)
            .and(Keyword::bm25("chunks", over_fetch))
            .fuse_rrf(60)
            .top(over_fetch);
        let hits: Vec<Hit> =
            self.lunaris.recall().with_root(plan).execute(Query::text(query)).await?;
        let prefix = self.thread_prefix.clone();
        let hits: Vec<Hit> = hits.into_iter().filter(|h| h.source.starts_with(&prefix)).collect();
        let now = self.lunaris.clock().tick();
        let mut rescored: Vec<(f32, Hit)> = hits
            .into_iter()
            .map(|h| {
                let age_s = hit_age_seconds(&h, now);
                // Blend ACT-R base-level with fused RRF — both are monotone
                // in "more signal = higher score"; a simple sum keeps
                // relative ordering intuitive for wrappers in Phase 10.
                let score = act_r_base_level(&[age_s]) + h.score;
                (score, h)
            })
            .collect();
        rescored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(rescored.into_iter().take(self.top_k).map(|(_, h)| h).collect())
    }
}

/// Extract age (seconds, f32) of a Hit's `valid_from` timestamp from `now`.
/// Clamps to [`MIN_AGE_SECONDS`] so `pow(-0.5)` never explodes when a hit is
/// ingested and recalled within the same HLC wall-millis (common in test
/// loops).
fn hit_age_seconds(hit: &Hit, now: Hlc) -> f32 {
    // `valid_from` is a bare `Hlc`, not `Option<Hlc>` — no unwrap needed.
    let delta_ms = now.wall_ms.saturating_sub(hit.valid_from.wall_ms);
    ((delta_ms as f32) / 1000.0).max(MIN_AGE_SECONDS)
}

/// ACT-R base-level activation per Anderson (1996).
///
/// `B(i) = ln(sum_k t_k^-d)` with `d = ACT_R_DECAY = 0.5`. `ages_s` is the
/// vector of prior-access ages in seconds. Returns `f32::NEG_INFINITY` on
/// empty input so callers can detect "no prior access" without panicking.
fn act_r_base_level(ages_s: &[f32]) -> f32 {
    if ages_s.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f32 = ages_s.iter().map(|t| t.powf(-ACT_R_DECAY)).sum();
    sum.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRIM-01 ≤ 30 LOC public-surface contract. Counts `pub fn` + `pub async fn`
    /// declarations in the production portion of the source file (everything
    /// BEFORE the first `#[cfg(test)]` marker). Mirrors the Phase 5
    /// `helios_scratchpad_public_surface_under_50_loc` template verbatim.
    #[test]
    fn message_stream_public_surface_under_30_loc() {
        let src = include_str!("./message_stream.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "PRIM-01 ≤30-LOC contract: MessageStream has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "PRIM-01 contract: MessageStream needs at least 3 public methods \
             (new, ingest, recall); got {pub_fns}"
        );
    }

    /// Numeric reference check against Anderson (1996) base-level formula.
    /// With ages `[1 s, 60 s, 3600 s]`, the scorer must match
    /// `ln(1^-0.5 + 60^-0.5 + 3600^-0.5)` within single-precision epsilon.
    #[test]
    fn message_stream_recency_scorer_matches_act_r_formula() {
        let ages = [1.0_f32, 60.0, 3600.0];
        let got = act_r_base_level(&ages);
        let expected = (1.0_f32.powf(-0.5) + 60.0_f32.powf(-0.5) + 3600.0_f32.powf(-0.5)).ln();
        assert!(
            (got - expected).abs() < 1e-5,
            "ACT-R formula mismatch: got {got}, expected {expected}"
        );
    }

    /// Monotonicity sanity — a 1-second-old access must score strictly higher
    /// than a 1-hour-old access under the base-level activation curve.
    #[test]
    fn message_stream_recency_scorer_more_recent_scores_higher() {
        let recent = act_r_base_level(&[1.0]);
        let old = act_r_base_level(&[3600.0]);
        assert!(recent > old, "recent={recent} should exceed old={old}");
    }

    /// Empty-input contract: no prior accesses means no activation. Returning
    /// `NEG_INFINITY` lets callers distinguish "no recency signal" from a
    /// valid-but-small score.
    #[test]
    fn message_stream_recency_scorer_empty_returns_neg_infinity() {
        let v = act_r_base_level(&[]);
        assert!(v.is_infinite() && v.is_sign_negative());
    }

    /// ACT-R decay constant sanity — guards against an accidental retuning
    /// that would desync [`act_r_base_level`] from the formula citation in
    /// this module's rustdoc + from `lunaris_consolidate::ActRScorer`.
    #[test]
    fn act_r_decay_constant_is_anderson_1996_half() {
        assert_eq!(ACT_R_DECAY, 0.5);
    }
}
