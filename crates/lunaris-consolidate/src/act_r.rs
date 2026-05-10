//! Plan 04-02 Task 2 — ACT-R activation scorer per D-13.
//!
//! Anderson, J.R. (1996) "ACT: A simple theory of complex cognition."
//! _American Psychologist_ 51(4): 355–365 — base-level activation:
//!
//! ```text
//! A_i = ln( Σ_j t_j^(-d) ) + S_i + ε
//! ```
//!
//! where `t_j` are the elapsed times since each of the `j` references to
//! fact `i`, `d` is the decay parameter (default 0.5 per Anderson 1996),
//! `S_i` is a spreading-activation term from graph neighbors (D-14 default
//! 1-hop), and `ε` is activation noise (D-13 default 0.0 in v0; non-zero is
//! v2 CONSOL-V2-01).
//!
//! Petrov, A. (2006) "Computationally efficient approximation of the
//! base-level learning equation in ACT-R." gives the O(1) incremental update:
//! cache the running `Σ_j t_j^(-d)` per Fact and add the new `t_new^(-d)`
//! term on each new reference instead of recomputing the full history.
//! [`ActRScorer::update_with_new_reference`] implements this update.
//!
//! ## Defaults (D-13)
//!
//! [`ActRScorer::with_defaults`] uses [`ConsolidatorConfig::default`]:
//! `decay=0.5`, `archive_threshold=-0.5`, `promote_threshold=+1.0`,
//! `spreading_depth=1`, `noise=0.0`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ulid::Ulid;

use lunaris_core::{LunarisError, StoragePort};

use crate::Consolidator;
use crate::types::{
    ArchiveEvent, ConsolidateEvent, ConsolidationReport, ConsolidatorConfig, FactId,
    PromotionEvent, ReferenceTime,
};

/// Anderson 1996 base-level activation scorer with Petrov 2006 O(1)
/// incremental approximation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActRScorer {
    config: ConsolidatorConfig,
}

impl ActRScorer {
    /// New scorer with the given config.
    pub fn new(config: ConsolidatorConfig) -> Self {
        Self { config }
    }

    /// New scorer with [`ConsolidatorConfig::default`] — D-13 reference
    /// values (`decay=0.5`, `archive=-0.5`, `promote=+1.0`, 1-hop, no noise).
    pub fn with_defaults() -> Self {
        Self::new(ConsolidatorConfig::default())
    }

    /// Current config (for test assertions).
    pub fn config(&self) -> &ConsolidatorConfig {
        &self.config
    }

    /// Anderson 1996 base-level activation: `A_i = ln(Σ_j t_j^(-d)) + S_i + ε`.
    ///
    /// Returns `f64::NEG_INFINITY` when `refs` is empty (`ln(0) = -∞`) so a
    /// never-referenced Fact always scores below `archive_threshold`.
    ///
    /// `elapsed_seconds` is floored at 1 before the `t^(-d)` exponentiation
    /// to avoid `0^-0.5 = ∞` blowing up the running sum when an event arrives
    /// in the same second as its reference. Matches ACT-R convention that
    /// activation grows with time-since-reference, with a sensible 1-second
    /// floor at the boundary.
    pub fn activation(&self, refs: &[ReferenceTime], spreading: f64) -> f64 {
        if refs.is_empty() {
            return f64::NEG_INFINITY;
        }
        let sum: f64 =
            refs.iter().map(|r| (r.elapsed_seconds.max(1) as f64).powf(-self.config.decay)).sum();
        sum.ln() + spreading + self.config.noise
    }

    /// Petrov 2006 O(1) incremental update. Given the previous
    /// `Σ_j t_j^(-d)` running sum (`prior_sum`) and a new reference arriving
    /// at `t_new` seconds ago, returns the updated sum. The caller then
    /// computes the new activation as `ln(updated_sum) + spreading + noise`.
    ///
    /// This is the production hot-path: the consolidator caches the running
    /// sum per Fact and avoids the O(refs.len()) full-history recompute on
    /// every new reference.
    pub fn update_with_new_reference(&self, prior_sum: f64, t_new: u64) -> f64 {
        prior_sum + (t_new.max(1) as f64).powf(-self.config.decay)
    }

    /// Returns `true` when `activation > promote_threshold` (D-13 default
    /// `+1.0`). The worker then issues an Episode→Fact promotion.
    pub fn should_promote(&self, activation: f64) -> bool {
        activation > self.config.promote_threshold
    }

    /// Returns `true` when `activation < archive_threshold` (D-13 default
    /// `-0.5`). The worker then archives the Fact out of the hot vector
    /// index into cold storage.
    pub fn should_archive(&self, activation: f64) -> bool {
        activation < self.config.archive_threshold
    }
}

impl Default for ActRScorer {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ============================================================================
// ActRConsolidator — Phase 16-00 authored `impl Consolidator` wrapping ActRScorer.
// ============================================================================

/// Deterministic [`FactId`] synthesized from an `episode_id` alone.
///
/// Per Plan 16-00 scope we do NOT read the Episode's subject/predicate/object
/// from storage to build a real [`FactId::from_triple`] — that would widen
/// 16-00 beyond "author the type" into storage-schema territory. The
/// consolidator produces a stable per-episode `FactId` via a namespaced
/// blake3 hash of the episode ULID bytes; later phases (CONSOL-V2) that wire
/// the real Episode→Fact triple can swap this for [`FactId::from_triple`]
/// without changing the audit envelope shape.
///
/// Namespace constant (`"lunaris-consolidate::episode-fact::"`) guarantees
/// the synthesized IDs never collide with triple-derived `FactId`s from the
/// extractor pipeline — the two hash inputs share no prefix.
fn synthesize_fact_id_from_episode(episode_id: Ulid) -> FactId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"lunaris-consolidate::episode-fact::");
    hasher.update(&episode_id.to_bytes());
    let h = hasher.finalize();
    let bytes: [u8; 16] =
        h.as_bytes()[..16].try_into().expect("blake3 output is 32 bytes; first 16 always fit");
    FactId(bytes)
}

/// Production ACT-R consolidator — composes [`ActRScorer`] into a concrete
/// `impl Consolidator` that the worker can iterate against.
///
/// ## Design (Plan 16-00)
///
/// Groups incoming [`ConsolidateEvent`]s by `episode_id`, treats each event
/// as one reference whose elapsed time derives from
/// `now_seconds - lsn_wall_ms / 1000` (floored at 1 per scorer convention),
/// scores via [`ActRScorer::activation`] with `spreading=0.0` (Leiden
/// spreading-activation wiring is deferred — see Gap below), and emits one
/// [`PromotionEvent`] or [`ArchiveEvent`] per episode whose activation
/// crosses the scorer's threshold. Episodes scoring in the neutral band
/// produce zero report entries.
///
/// ## Audit emission ownership (CONSOL-V1-05)
///
/// `ActRConsolidator::consolidate` does NOT call `publish_audit_event`
/// directly. Per the crate-root docstring (B-1: D-22 verbatim) the
/// worker iterates `report.promotions` / `report.archives` and publishes
/// one audit envelope per event via `lunaris_core::audit::publish_audit_event`.
/// This keeps [`NoopConsolidator`] and [`ActRConsolidator`] on identical
/// emission contracts and leaves the `no_inline_audit_envelopes.rs` grep-gate
/// green by construction — no new emission sites added.
///
/// ## Known gap (Leiden spreading activation)
///
/// D-14 specifies spreading activation from 1-hop graph neighbors as an
/// additive term in the Anderson 1996 formula. Building the [`GraphSnapshot`]
/// requires `StoragePort::graph_traverse`; wiring that read is deferred to a
/// follow-up plan (tracked alongside CONSOL-V2-01 in REQUIREMENTS.md). Until
/// then `spreading=0.0` matches the scorer's `noise=0.0` default — the
/// activation is `ln(Σ t_j^(-d))` with no extra terms, which is Anderson 1996
/// reduced to its base-level-only form.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActRConsolidator {
    scorer: ActRScorer,
}

impl ActRConsolidator {
    /// New consolidator with the given scorer.
    pub fn new(scorer: ActRScorer) -> Self {
        Self { scorer }
    }

    /// Current scorer config (exposed for test assertions + ops introspection).
    pub fn config(&self) -> &ConsolidatorConfig {
        self.scorer.config()
    }

    /// Classify one episode's debounced event batch into `promote` / `archive`
    /// / `noop`. Extracted for unit testing + for an eventual Criterion bench
    /// that exercises scoring in isolation from storage I/O.
    fn classify_episode(&self, now_secs: u64, events: &[ConsolidateEvent]) -> EpisodeDecision {
        if events.is_empty() {
            return EpisodeDecision::Noop;
        }
        let episode_id = events[0].episode_id;
        let refs: Vec<ReferenceTime> = events
            .iter()
            .map(|e| {
                let event_secs = e.lsn_wall_ms / 1000;
                let elapsed = now_secs.saturating_sub(event_secs);
                ReferenceTime::new(elapsed)
            })
            .collect();
        let activation = self.scorer.activation(&refs, 0.0);
        let fact_id = synthesize_fact_id_from_episode(episode_id);
        if self.scorer.should_promote(activation) {
            EpisodeDecision::Promote(PromotionEvent {
                episode_id,
                fact_id,
                activation_score: activation,
            })
        } else if self.scorer.should_archive(activation) {
            EpisodeDecision::Archive(ArchiveEvent {
                fact_id,
                final_activation: activation,
                moved_to: "cold_storage".to_string(),
            })
        } else {
            EpisodeDecision::Noop
        }
    }
}

/// Internal classification result from [`ActRConsolidator::classify_episode`].
#[derive(Clone, Debug, PartialEq)]
enum EpisodeDecision {
    Promote(PromotionEvent),
    Archive(ArchiveEvent),
    Noop,
}

#[async_trait]
impl Consolidator for ActRConsolidator {
    fn debug_name(&self) -> &'static str {
        "ActRConsolidator"
    }

    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        if events.is_empty() {
            return Ok(ConsolidationReport::default());
        }

        // "now" in whole seconds since UNIX epoch; floor-safe when SystemTime
        // is pre-epoch (extremely unlikely, but don't panic on a misconfigured
        // clock).
        let now_secs =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        // Group by episode_id so multiple debounced events for the same
        // episode coalesce into one scoring pass — matches the worker's
        // HashMap<Ulid, Vec<ConsolidateEvent>> debounce buffer shape.
        let mut by_episode: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        for e in events {
            by_episode.entry(e.episode_id).or_default().push(e.clone());
        }

        // Deterministic iteration order (by episode_id byte order) so test
        // assertions on promotions/archives vector order are stable across
        // runs.
        let mut episode_ids: Vec<Ulid> = by_episode.keys().copied().collect();
        episode_ids.sort_by_key(|u| u.to_bytes());

        let mut report = ConsolidationReport::default();
        for ep in episode_ids {
            let batch = &by_episode[&ep];
            match self.classify_episode(now_secs, batch) {
                EpisodeDecision::Promote(p) => report.promotions.push(p),
                EpisodeDecision::Archive(a) => report.archives.push(a),
                EpisodeDecision::Noop => {}
            }
        }
        Ok(report)
    }

    fn applies(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: Anderson 1996 formula for two references at t=60, t=120 with
    /// d=0.5 and no spreading / noise: `A = ln(60^-0.5 + 120^-0.5)`.
    #[test]
    fn activation_matches_anderson_1996_two_refs() {
        let s = ActRScorer::with_defaults();
        let refs = [ReferenceTime::new(60), ReferenceTime::new(120)];
        let a = s.activation(&refs, 0.0);
        // 60^-0.5 ≈ 0.12910, 120^-0.5 ≈ 0.09129, sum ≈ 0.22039, ln ≈ -1.5125
        let expected = (60_f64.powf(-0.5) + 120_f64.powf(-0.5)).ln();
        assert!((a - expected).abs() < 1e-9, "activation = {a}, expected = {expected}");
        // Cross-check the approximate literal from the plan:
        assert!((a - (-1.514)).abs() < 1e-2, "activation ~= -1.514; got {a}");
    }

    /// Test 2: Empty refs → `-∞`. The scorer's archive predicate will then
    /// trigger for any finite `archive_threshold`, which is every real config.
    #[test]
    fn activation_is_neg_infinity_on_empty_refs() {
        let s = ActRScorer::with_defaults();
        let a = s.activation(&[], 0.0);
        assert!(a.is_infinite() && a.is_sign_negative());
    }

    /// Test 3: Petrov 2006 incremental update equals the full-history
    /// recompute. Plan literal sanity check: `update(0.220, 30) ≈ 0.403`.
    #[test]
    fn petrov_2006_incremental_update_matches_forward_pass() {
        let s = ActRScorer::with_defaults();
        let prior = 60_f64.powf(-0.5); // ≈ 0.12910
        let updated = s.update_with_new_reference(prior, 30);
        let expected = prior + 30_f64.powf(-0.5);
        assert!((updated - expected).abs() < 1e-12);
        // The incremental path equals the full-history recompute:
        let full = s.activation(&[ReferenceTime::new(60), ReferenceTime::new(30)], 0.0);
        let incremental = updated.ln();
        assert!((full - incremental).abs() < 1e-12);

        // Plan literal: starting from a synthetic prior_sum=0.220 + new ref
        // t=30 (30^-0.5 ≈ 0.18257) → ≈ 0.40257.
        let updated_literal = s.update_with_new_reference(0.220, 30);
        assert!(
            (updated_literal - 0.40257).abs() < 1e-3,
            "plan literal expects ~0.403; got {updated_literal}"
        );
    }

    /// Test 4: `should_promote(1.5)==true` with default threshold 1.0.
    #[test]
    fn should_promote_fires_above_default_threshold() {
        let s = ActRScorer::with_defaults();
        assert!(s.should_promote(1.5));
        assert!(!s.should_promote(1.0), "strictly greater than, not >=");
        assert!(!s.should_promote(0.9));
    }

    /// Test 5: `should_archive(-0.7)==true` with default threshold -0.5.
    #[test]
    fn should_archive_fires_below_default_threshold() {
        let s = ActRScorer::with_defaults();
        assert!(s.should_archive(-0.7));
        assert!(!s.should_archive(-0.5), "strictly less than, not <=");
        assert!(!s.should_archive(-0.3));
    }

    /// Test 6: Spreading activation parameter is additive.
    #[test]
    fn spreading_activation_is_additive() {
        let s = ActRScorer::with_defaults();
        let refs = [ReferenceTime::new(60), ReferenceTime::new(120)];
        let base = s.activation(&refs, 0.0);
        let with_spread = s.activation(&refs, 1.25);
        assert!((with_spread - base - 1.25).abs() < 1e-12);
    }

    /// Test 7: Elapsed-seconds floor at 1 prevents `0^-0.5 = ∞` from
    /// blowing up the sum when an event arrives in the same second as its
    /// reference.
    #[test]
    fn zero_elapsed_seconds_floors_to_one() {
        let s = ActRScorer::with_defaults();
        let a = s.activation(&[ReferenceTime::new(0)], 0.0);
        // ln(1^-0.5) + 0 + 0 = ln(1) = 0
        assert!(a.abs() < 1e-12, "activation with floored t=1 = {a}, expected 0");
    }

    // -----------------------------------------------------------------
    // Plan 16-00 Task 2 GREEN — ActRConsolidator unit tests.
    // Assert on the returned ConsolidationReport directly (the worker —
    // not the consolidator — owns audit emission per CONSOL-V1-05).
    // -----------------------------------------------------------------

    /// Helper — minimal never-called StoragePort so `consolidate` can be
    /// driven without a live backend. `ActRConsolidator::consolidate` does
    /// not touch storage in the Plan 16-00 scope (Leiden spreading-activation
    /// read is deferred) so the port's methods are unreachable in unit tests.
    #[derive(Default)]
    struct UnusedStorage;

    #[async_trait::async_trait]
    impl lunaris_core::StoragePort for UnusedStorage {
        async fn atomic_write(
            &self,
            _scope: &lunaris_core::Scope,
            _ops: &[lunaris_core::WriteOp],
        ) -> Result<lunaris_core::Lsn, lunaris_core::StorageError> {
            Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
        }
        async fn vector_search(
            &self,
            _scope: &lunaris_core::Scope,
            _index: &str,
            _query: &[f32],
            _k: usize,
            _filter: Option<&lunaris_core::Filter>,
            _as_of: Option<lunaris_core::Hlc>,
            _rerank: bool,
        ) -> Result<Vec<lunaris_core::VectorHit>, lunaris_core::StorageError> {
            Ok(Vec::new())
        }
        async fn graph_traverse(
            &self,
            _scope: &lunaris_core::Scope,
            _q: &lunaris_core::CypherQuery,
            _as_of: Option<lunaris_core::Hlc>,
        ) -> Result<lunaris_core::GraphResult, lunaris_core::StorageError> {
            Ok(lunaris_core::GraphResult::default())
        }
        async fn scan_range(
            &self,
            _scope: &lunaris_core::Scope,
            _prefix: &[u8],
            _as_of: Option<lunaris_core::Hlc>,
        ) -> Result<
            futures::stream::BoxStream<
                '_,
                Result<(bytes::Bytes, bytes::Bytes), lunaris_core::StorageError>,
            >,
            lunaris_core::StorageError,
        > {
            Ok(Box::pin(futures::stream::iter(Vec::new())))
        }
        async fn read_as_of(
            &self,
            _scope: &lunaris_core::Scope,
            _key: &[u8],
            _as_of: lunaris_core::Hlc,
        ) -> Result<Option<lunaris_core::Row<bytes::Bytes>>, lunaris_core::StorageError> {
            Ok(None)
        }
        async fn publish(
            &self,
            _scope: &lunaris_core::Scope,
            _topic: &str,
            _partition: u16,
            _payload: bytes::Bytes,
        ) -> Result<u64, lunaris_core::StorageError> {
            Ok(0)
        }
        async fn subscribe(
            &self,
            _scope: &lunaris_core::Scope,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<lunaris_core::QueueMsg, lunaris_core::StorageError>,
            >,
            lunaris_core::StorageError,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
        fn capabilities(&self) -> lunaris_core::StorageCapabilities {
            lunaris_core::StorageCapabilities {
                bi_temporal_native: false,
                graph_native: false,
                rerank_native: false,
                queue_native: false,
                max_vector_dim: 768,
                native_rrf: false,
            }
        }
    }

    fn event_for(episode_id: Ulid, lsn_wall_ms: u64, lsn_counter: u64) -> ConsolidateEvent {
        ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id,
            lsn_wall_ms,
            lsn_counter,
            source: "helios:fs/test".into(),
        }
    }

    /// Promote-path: single episode with many recent references lands above
    /// the `promote_threshold=+1.0` and produces one `PromotionEvent` in the
    /// report. The synthesized `FactId` is deterministic from the episode id.
    ///
    /// Many identical references at elapsed=1s → sum = N * 1^-0.5 = N.
    /// `ln(N) > 1.0` requires `N > e ≈ 2.72`, so 16 references are safely
    /// above threshold (`ln(16) ≈ 2.77`). We pin `now_secs` via the public
    /// `classify_episode` helper so the test is not wall-clock-dependent.
    #[test]
    fn act_r_consolidator_promotes_above_threshold() {
        let c = ActRConsolidator::default();
        let ep = Ulid::from_bytes([1; 16]);
        let now_secs = 1_000_000u64;
        // All events at elapsed=1s (lsn_wall_ms = (now_secs - 1) * 1000).
        let events: Vec<ConsolidateEvent> =
            (0..16).map(|i| event_for(ep, (now_secs - 1) * 1000, i)).collect();
        let d = c.classify_episode(now_secs, &events);
        match d {
            EpisodeDecision::Promote(p) => {
                assert_eq!(p.episode_id, ep);
                assert_eq!(p.fact_id, synthesize_fact_id_from_episode(ep));
                assert!(
                    p.activation_score > c.config().promote_threshold,
                    "activation {} must exceed promote threshold {}",
                    p.activation_score,
                    c.config().promote_threshold
                );
            }
            other => panic!("expected Promote, got {other:?}"),
        }
    }

    /// Archive-path: single episode with a single very-stale reference scores
    /// below `archive_threshold=-0.5` and produces one `ArchiveEvent`.
    /// `ln(t^-0.5) = -0.5 * ln(t)`; for archive we need `-0.5 * ln(t) < -0.5`
    /// i.e. `ln(t) > 1` i.e. `t > e`. Pick `elapsed=3600s` (one hour) →
    /// `-0.5 * ln(3600) ≈ -4.094`, comfortably below -0.5.
    #[test]
    fn act_r_consolidator_archives_below_threshold() {
        let c = ActRConsolidator::default();
        let ep = Ulid::from_bytes([2; 16]);
        let now_secs = 1_000_000u64;
        let events = vec![event_for(ep, (now_secs - 3600) * 1000, 0)];
        let d = c.classify_episode(now_secs, &events);
        match d {
            EpisodeDecision::Archive(a) => {
                assert_eq!(a.fact_id, synthesize_fact_id_from_episode(ep));
                assert!(
                    a.final_activation < c.config().archive_threshold,
                    "activation {} must be below archive threshold {}",
                    a.final_activation,
                    c.config().archive_threshold
                );
                assert_eq!(a.moved_to, "cold_storage");
            }
            other => panic!("expected Archive, got {other:?}"),
        }
    }

    /// Noop-path: single episode whose activation lands in the neutral band
    /// (between archive -0.5 and promote +1.0) produces no report entry.
    /// Two references at elapsed=60s + 120s → `activation ≈ -1.514` is below
    /// -0.5 (archive). So we pick three references such that activation
    /// lands between -0.5 and +1.0: e.g. all at elapsed=1s → ln(3) ≈ 1.099
    /// which is promote; elapsed=2s three times → ln(3 * 2^-0.5) =
    /// ln(2.121) ≈ 0.752 — neutral band.
    #[test]
    fn act_r_consolidator_noop_between_thresholds() {
        let c = ActRConsolidator::default();
        let ep = Ulid::from_bytes([3; 16]);
        let now_secs = 1_000_000u64;
        let events: Vec<ConsolidateEvent> =
            (0..3).map(|i| event_for(ep, (now_secs - 2) * 1000, i)).collect();
        let d = c.classify_episode(now_secs, &events);
        assert_eq!(d, EpisodeDecision::Noop, "mid-band activation must not emit");
    }

    /// Empty-batch path: `consolidate([])` returns the empty report without
    /// hitting the scorer. Ensures the hot-path short-circuit stays cheap.
    #[tokio::test]
    async fn act_r_consolidator_empty_batch_returns_empty_report() {
        let c = ActRConsolidator::default();
        let s: std::sync::Arc<dyn lunaris_core::StoragePort> = std::sync::Arc::new(UnusedStorage);
        let report = c.consolidate(s, &[]).await.unwrap();
        assert!(report.promotions.is_empty());
        assert!(report.archives.is_empty());
        assert_eq!(report.communities_rebuilt, 0);
    }

    /// Full `consolidate` pass over a multi-episode batch: promotions and
    /// archives are grouped by episode_id, output vectors are sorted by
    /// episode_id bytes (deterministic ordering regression guard).
    #[tokio::test]
    async fn act_r_consolidator_groups_by_episode_id_deterministically() {
        let c = ActRConsolidator::default();
        let s: std::sync::Arc<dyn lunaris_core::StoragePort> = std::sync::Arc::new(UnusedStorage);
        let ep_a = Ulid::from_bytes([0x0A; 16]);
        let ep_b = Ulid::from_bytes([0x0B; 16]);
        // 16 refs at elapsed=1s for each of ep_a and ep_b -> both promote.
        // Derive lsn_wall_ms from a recent "now" so elapsed ≈ 1s regardless of
        // wall clock drift during the test.
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let lsn_ms = now_ms.saturating_sub(1_000);
        let mut batch: Vec<ConsolidateEvent> = Vec::new();
        for ep in [ep_b, ep_a] {
            // reversed insertion order to prove sort is by id, not input order
            for i in 0..16 {
                batch.push(event_for(ep, lsn_ms, i));
            }
        }
        let report = c.consolidate(s, &batch).await.unwrap();
        assert_eq!(report.promotions.len(), 2, "both episodes promote");
        assert_eq!(report.promotions[0].episode_id, ep_a, "ep_a (0x0A) sorts before ep_b (0x0B)");
        assert_eq!(report.promotions[1].episode_id, ep_b);
        assert!(report.archives.is_empty());
    }

    /// Test 8: Custom config — swap decay / thresholds — flows through every
    /// predicate.
    #[test]
    fn custom_config_overrides_flow_through_predicates() {
        let cfg = ConsolidatorConfig {
            decay: 0.3,
            archive_threshold: -1.0,
            promote_threshold: 2.5,
            spreading_depth: 2,
            noise: 0.1,
            community_rebuild_delta: 0.10,
        };
        let s = ActRScorer::new(cfg);
        assert_eq!(s.config().decay, 0.3);
        assert_eq!(s.config().promote_threshold, 2.5);
        assert!(!s.should_promote(2.0));
        assert!(s.should_promote(3.0));
        assert!(s.should_archive(-1.5));
        assert!(!s.should_archive(-0.75));
        // Activation respects the custom decay + noise terms:
        let a = s.activation(&[ReferenceTime::new(10)], 0.0);
        let expected = (10_f64.powf(-0.3)).ln() + 0.0 + 0.1;
        assert!((a - expected).abs() < 1e-12);
    }
}
