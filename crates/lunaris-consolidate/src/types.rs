//! Consolidator DTOs.
//!
//! ## B-1 fix (D-22 verbatim)
//!
//! [`ConsolidationReport`] carries per-event vectors — `promotions:
//! Vec<PromotionEvent>` + `archives: Vec<ArchiveEvent>` — so the worker emits
//! ONE `AuditEvent::ConsolidatorPromotion` per promoted Episode→Fact AND ONE
//! `AuditEvent::ConsolidatorArchive` per archived Fact. The previous rolled-up
//! `promoted: u64, archived: u64` counts are REMOVED — Plan 04-05's
//! `AuditEvent` enum has no `ConsolidatorReport` variant.
//!
//! ## Deterministic IDs (D-13)
//!
//! [`FactId`] + [`CommunityId`] are 16-byte blake3 content hashes mirroring
//! [`lunaris_extract::EntityId`] verbatim:
//!
//! - `FactId = blake3(subject_id || "::" || predicate || "::" || object_id)[..16]`
//! - `CommunityId = blake3(sorted_member_ids.join("::"))[..16]`
//!
//! 16 bytes = 128 bits of entropy; collision probability ≈ 2^-64 at ~1B ids.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use lunaris_extract::EntityId;

// ------------------------------- FactId --------------------------------------

/// Deterministic 16-byte content-hash identifier for promoted Facts.
///
/// ```text
/// FactId = blake3(subject_id.0 || "::" || predicate.as_bytes() || "::" || object_id.0)[..16]
/// ```
///
/// The `Display` impl renders lowercase hex (32 chars) so a `FactId` can be
/// embedded verbatim in Cypher `WHERE n.id = '...'` literals and in tracing
/// spans without an extra format step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactId(pub [u8; 16]);

impl FactId {
    /// Derive a deterministic [`FactId`] from a `(subject, predicate, object)`
    /// triple. Byte-identical across runs for the same input (proven by
    /// `fact_id_is_deterministic`); distinguishes predicate case (proven by
    /// `fact_id_distinguishes_predicate_case`) and subject↔object order
    /// (proven by `fact_id_distinguishes_subject_object_order`).
    pub fn from_triple(subject: EntityId, predicate: &str, object: EntityId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&subject.0);
        hasher.update(b"::");
        hasher.update(predicate.as_bytes());
        hasher.update(b"::");
        hasher.update(&object.0);
        let h = hasher.finalize();
        let bytes: [u8; 16] = h.as_bytes()[..16]
            .try_into()
            .expect("blake3 output is 32 bytes; first 16 always fit");
        FactId(bytes)
    }
}

impl std::fmt::Display for FactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

// ----------------------------- CommunityId -----------------------------------

/// Deterministic 16-byte content-hash identifier for Leiden communities.
///
/// ```text
/// CommunityId = blake3(sorted_member_ids.join("::"))[..16]
/// ```
///
/// Order-insensitive by construction (members are sorted before hashing) so a
/// community is identified by its MEMBER SET, not insertion order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommunityId(pub [u8; 16]);

impl CommunityId {
    /// Derive a deterministic [`CommunityId`] from a member set. Sorts the
    /// input before hashing so `from_members(&[a, b])` and `from_members(&[b,
    /// a])` produce byte-identical ids (proven by
    /// `community_id_is_order_insensitive`). Different member sets produce
    /// distinct ids (proven by `community_id_distinguishes_member_set`).
    pub fn from_members(members: &[EntityId]) -> Self {
        let mut sorted: Vec<&EntityId> = members.iter().collect();
        sorted.sort_by_key(|e| &e.0);
        let mut hasher = blake3::Hasher::new();
        let mut first = true;
        for m in sorted {
            if !first {
                hasher.update(b"::");
            }
            hasher.update(&m.0);
            first = false;
        }
        let h = hasher.finalize();
        let bytes: [u8; 16] = h.as_bytes()[..16]
            .try_into()
            .expect("blake3 output is 32 bytes; first 16 always fit");
        CommunityId(bytes)
    }
}

impl std::fmt::Display for CommunityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

// --------------------------- ConsolidatorConfig ------------------------------

/// Runtime knobs for the ACT-R scorer + Leiden community rebuild.
///
/// Defaults match the D-13 Anderson 1996 reference values verbatim:
///
/// | Field | Default | Source |
/// |---|---|---|
/// | `decay` | 0.5 | Anderson 1996 `d` parameter |
/// | `archive_threshold` | -0.5 | D-13 archive-out-of-hot-index threshold |
/// | `promote_threshold` | 1.0 | D-13 Episode→Fact promotion threshold |
/// | `spreading_depth` | 1 | D-14 — multi-hop is v2 CONSOL-V2-01 |
/// | `noise` | 0.0 | D-13 — non-zero is v2 |
/// | `community_rebuild_delta` | 0.05 | D-17 — rebuild on ±5% edge-count delta |
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidatorConfig {
    pub decay: f64,
    pub archive_threshold: f64,
    pub promote_threshold: f64,
    pub spreading_depth: u8,
    pub noise: f64,
    pub community_rebuild_delta: f64,
}

impl Default for ConsolidatorConfig {
    fn default() -> Self {
        Self {
            decay: 0.5,
            archive_threshold: -0.5,
            promote_threshold: 1.0,
            spreading_depth: 1,
            noise: 0.0,
            community_rebuild_delta: 0.05,
        }
    }
}

// ----------------------------- ReferenceTime ---------------------------------

/// Elapsed-time handle used by the ACT-R scorer. Carries seconds-since-last-
/// reference so the scorer can compute `t_j^(-d)` without worrying about
/// epochs / time zones / wall vs monotonic clocks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceTime {
    pub elapsed_seconds: u64,
}

impl ReferenceTime {
    pub fn new(elapsed_seconds: u64) -> Self {
        Self { elapsed_seconds }
    }
}

// ----------------------------- PromotionEvent --------------------------------

/// B-1 fix: per-promotion event matching D-22 verbatim.
///
/// One of these is produced for every Episode→Fact promotion the consolidator
/// decides. The worker iterates `ConsolidationReport::promotions` and emits
/// ONE `AuditEvent::ConsolidatorPromotion { episode_id, fact_id,
/// activation_score }` per event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromotionEvent {
    pub episode_id: Ulid,
    pub fact_id: FactId,
    pub activation_score: f64,
}

// ------------------------------ ArchiveEvent ---------------------------------

/// B-1 fix: per-archive event matching D-22 verbatim.
///
/// One of these is produced for every archived Fact. The worker iterates
/// `ConsolidationReport::archives` and emits ONE
/// `AuditEvent::ConsolidatorArchive { fact_id, final_activation, moved_to }`
/// per event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchiveEvent {
    pub fact_id: FactId,
    pub final_activation: f64,
    pub moved_to: String,
}

// --------------------------- ConsolidationReport -----------------------------

/// B-1 fix: worker output carries per-event vectors so the audit emit path
/// publishes ONE event per promotion + ONE per archive. The previous rolled-up
/// `promoted: u64, archived: u64` counts are GONE — Plan 04-05's `AuditEvent`
/// enum has no `ConsolidatorReport` variant to carry them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationReport {
    pub promotions: Vec<PromotionEvent>,
    pub archives: Vec<ArchiveEvent>,
    /// Number of community rebuilds triggered this pass (CONSOL-04). v0 has no
    /// per-rebuild audit variant; this is a count for ops visibility via
    /// `tracing::info!` only.
    pub communities_rebuilt: u64,
}

// ---------------------------- ConsolidateEvent -------------------------------

/// Wire shape of the envelope produced by Plan 04-04's `ingest.rs`
/// `publish_consolidate_event` helper (D-16 event-driven trigger).
///
/// ```json
/// {
///   "kind": "ingest_committed",
///   "episode_id": "<ulid>",
///   "lsn_wall_ms": ...,
///   "lsn_counter": ...
/// }
/// ```
///
/// The worker's debounce buffer groups events by `episode_id` so multiple
/// MVCC writes on the same Episode within the debounce window (D-17 default
/// 60 s) coalesce into ONE activation-scoring pass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsolidateEvent {
    /// Event tag. v0 uses `"ingest_committed"`; v1 may add
    /// `"forget_committed"` / `"verify_superseded"` variants.
    pub kind: String,
    pub episode_id: Ulid,
    pub lsn_wall_ms: u64,
    pub lsn_counter: u64,
    /// Phase 9.1 Plan 01 (PRIM-04 full wiring) — Episode `source` captured at
    /// publish-time by `crates/lunaris/src/ingest.rs::publish_consolidate_event`.
    /// Consumed by [`crate::Consolidator::consolidate_scoped`] (default method
    /// on the trait) to filter a batch by `event.source.starts_with(prefix)`
    /// before delegating to [`crate::Consolidator::consolidate`].
    ///
    /// `#[serde(default)]` is MANDATORY so in-flight `__lunaris_consolidate__`
    /// queue payloads from before this field existed deserialize as
    /// `source: ""`. Under any non-empty scope prefix an empty `source`
    /// fails the `starts_with` check and the event is dropped — the default
    /// `consolidate_scoped` impl fails closed by design (T-09-1-01-02).
    #[serde(default)]
    pub source: String,
}

// ------------------------------- IndexKind -----------------------------------

/// Tag for which storage index a consolidation action touched. Mirrors the
/// `lunaris::forget::IndexKind` enum shape (Plan 04-05) so Plan 04-04 can wire
/// them 1:1 when the audit emit surface is unified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    Kv,
    Vector,
    Graph,
}

// ---------------------------------- Tests ------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entity_id(n: u8) -> EntityId {
        EntityId([n; 16])
    }

    #[test]
    fn fact_id_is_deterministic() {
        let a = FactId::from_triple(entity_id(1), "knows", entity_id(2));
        let b = FactId::from_triple(entity_id(1), "knows", entity_id(2));
        assert_eq!(a, b, "same triple MUST produce byte-identical FactId");
        assert_eq!(a.to_string(), b.to_string());
        assert_eq!(a.to_string().len(), 32, "16 bytes → 32 hex chars");
    }

    #[test]
    fn fact_id_distinguishes_predicate_case() {
        let a = FactId::from_triple(entity_id(1), "knows", entity_id(2));
        let b = FactId::from_triple(entity_id(1), "KNOWS", entity_id(2));
        assert_ne!(a, b, "predicate is case-sensitive (predicates are canonical lowercase by convention; this guards against accidental case drift)");
    }

    #[test]
    fn fact_id_distinguishes_subject_object_order() {
        let a = FactId::from_triple(entity_id(1), "knows", entity_id(2));
        let b = FactId::from_triple(entity_id(2), "knows", entity_id(1));
        assert_ne!(a, b, "(subject, object) order MUST matter for FactId — directed edges");
    }

    #[test]
    fn community_id_is_order_insensitive() {
        let a = CommunityId::from_members(&[entity_id(1), entity_id(2), entity_id(3)]);
        let b = CommunityId::from_members(&[entity_id(3), entity_id(1), entity_id(2)]);
        assert_eq!(a, b, "community is identified by MEMBER SET, not insertion order");
    }

    #[test]
    fn community_id_distinguishes_member_set() {
        let a = CommunityId::from_members(&[entity_id(1), entity_id(2)]);
        let b = CommunityId::from_members(&[entity_id(1), entity_id(3)]);
        assert_ne!(a, b, "different member sets MUST produce different ids");
    }

    #[test]
    fn community_id_display_is_lowercase_hex() {
        let a = CommunityId([0xAB; 16]);
        let s = a.to_string();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn consolidator_config_defaults_match_anderson_1996() {
        let c = ConsolidatorConfig::default();
        assert_eq!(c.decay, 0.5, "D-13: Anderson 1996 default decay");
        assert_eq!(c.archive_threshold, -0.5, "D-13: reference archive threshold");
        assert_eq!(c.promote_threshold, 1.0, "D-13: reference Episode→Fact promote threshold");
        assert_eq!(c.spreading_depth, 1, "D-14: multi-hop is v2");
        assert_eq!(c.noise, 0.0, "D-13: non-zero noise is v2");
        assert_eq!(c.community_rebuild_delta, 0.05, "D-17: ±5% edge-count delta gates rebuild");
    }

    #[test]
    fn consolidation_report_default_is_all_empty() {
        let r = ConsolidationReport::default();
        assert!(r.promotions.is_empty());
        assert!(r.archives.is_empty());
        assert_eq!(r.communities_rebuilt, 0);
    }

    #[test]
    fn consolidate_event_round_trips_serde() {
        let e = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 12_345,
            lsn_counter: 7,
            source: String::new(),
        };
        let bytes = serde_json::to_vec(&e).unwrap();
        let back: ConsolidateEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(e.episode_id, back.episode_id);
        assert_eq!(e.lsn_wall_ms, back.lsn_wall_ms);
        assert_eq!(e.lsn_counter, back.lsn_counter);
        assert_eq!(e.kind, back.kind);
        assert_eq!(e.source, back.source);
    }

    /// Phase 9.1 Plan 01 Task 1 RED — legacy `ConsolidateEvent` JSON payloads
    /// (from before the `source` field existed) MUST still deserialize,
    /// defaulting `source` to `""`. Without `#[serde(default)]` this test
    /// fails with a "missing field `source`" serde error; with
    /// `#[serde(default)]` the default `String` (empty) fills in and the test
    /// passes. Locks the backward-compat contract on the
    /// `__lunaris_consolidate__` queue wire shape.
    #[test]
    fn consolidate_event_legacy_payload_without_source_deserializes() {
        let legacy_json = format!(
            r#"{{"kind":"ingest_committed","episode_id":"{}","lsn_wall_ms":1,"lsn_counter":2}}"#,
            Ulid::new()
        );
        let back: ConsolidateEvent = serde_json::from_str(&legacy_json)
            .expect("legacy payload without source field MUST deserialize");
        assert_eq!(back.kind, "ingest_committed");
        assert_eq!(back.lsn_wall_ms, 1);
        assert_eq!(back.lsn_counter, 2);
        assert_eq!(back.source, "", "missing source field defaults to empty string");
    }

    /// Phase 9.1 Plan 01 Task 1 — full payload with `source` field round-trips
    /// verbatim.
    #[test]
    fn consolidate_event_full_payload_with_source_round_trips() {
        let ep = Ulid::new();
        let full_json = format!(
            r#"{{"kind":"ingest_committed","episode_id":"{ep}","lsn_wall_ms":10,"lsn_counter":20,"source":"helios:fs/foo"}}"#,
        );
        let back: ConsolidateEvent = serde_json::from_str(&full_json).unwrap();
        assert_eq!(back.source, "helios:fs/foo");
    }

    /// B-1: `PromotionEvent` matches D-22 verbatim — the worker serializes
    /// `{ episode_id, fact_id, activation_score }` into the
    /// `AuditEvent::ConsolidatorPromotion` envelope.
    #[test]
    fn promotion_event_round_trips_serde() {
        let p = PromotionEvent {
            episode_id: Ulid::new(),
            fact_id: FactId::from_triple(entity_id(1), "knows", entity_id(2)),
            activation_score: 1.5,
        };
        let bytes = serde_json::to_vec(&p).unwrap();
        let back: PromotionEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(p, back);
    }

    /// B-1: `ArchiveEvent` matches D-22 verbatim — the worker serializes
    /// `{ fact_id, final_activation, moved_to }` into the
    /// `AuditEvent::ConsolidatorArchive` envelope.
    #[test]
    fn archive_event_round_trips_serde() {
        let a = ArchiveEvent {
            fact_id: FactId::from_triple(entity_id(3), "knows", entity_id(4)),
            final_activation: -0.7,
            moved_to: "cold_storage".to_string(),
        };
        let bytes = serde_json::to_vec(&a).unwrap();
        let back: ArchiveEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn index_kind_round_trips_serde() {
        for k in [IndexKind::Kv, IndexKind::Vector, IndexKind::Graph] {
            let bytes = serde_json::to_vec(&k).unwrap();
            let back: IndexKind = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(k, back);
        }
    }
}
