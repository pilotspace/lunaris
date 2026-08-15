//! `memory.dream_agenda` engine — engram-soul-loop **task 8a**
//! (`.add/tasks/dream-agenda/TASK.md` §3 CONTRACT, frozen).
//!
//! **READ-ONLY.** Tin's locked decision: the coding harness (Claude, not
//! Lunaris) is the distiller/judge. This module only SURFACES candidate
//! clusters of ripe raw episodes with activation stats — it never writes an
//! episode, a ledger row, or an agenda row. `grep -c atomic_write` on this
//! file MUST be `0` (task 8a §6 VERIFY).
//!
//! ## Pipeline
//!
//! 1. Candidates = every `(ulid, ActivationRecord)` in the scope's
//!    persistent activation ledger ([`LedgerReferenceSource::scan`]).
//! 2. Hydrate each candidate's episode ([`lunaris_core::Episode`] at
//!    [`lunaris_core::keyspace::episode_key`]); exclude a gone episode, a
//!    `distilled:*` source (never re-distill a distilled record), and — once
//!    `max_activation` is set — anything not yet ripe (decayed) enough.
//! 3. Cluster the survivors two ways, deterministically:
//!    - **Leiden path** — scan `fact_prefix(scope)`, recover each
//!      candidate-owned fact's `(subject_id, object_id, source_episode_id)`,
//!      build an intra-episode entity co-occurrence [`GraphSnapshot`], run
//!      [`leiden_pass`], and place each entity-bearing episode into the
//!      community of its lowest-byte-order entity.
//!    - **Source-class fallback** — episodes with zero entity signal group
//!      by the `source` prefix before the first `:`.
//! 4. Drop clusters under `min_cluster_size`, sort by size DESC then
//!    `mean_activation` DESC, cap to `limit`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt;
use lunaris_core::keyspace::{episode_key, fact_prefix};
use lunaris_core::{ConsolError, Hlc, LunarisError, Scope, StoragePort};
use lunaris_extract::EntityId;
use serde::Deserialize;
use ulid::Ulid;

use crate::ledger_reference_source::LedgerReferenceSource;
use crate::leiden::{GraphSnapshot, leiden_pass};
use crate::types::CommunityId;

/// Max label-propagation iterations for the dream-agenda leiden pass.
/// Scope-sized graphs are small and converge in a handful of iterations
/// (mirrors the bound the `leiden_pass` unit tests already exercise).
const MAX_LEIDEN_ITERS: usize = 50;

/// Snippet cap — mirrors `lunaris-memory-service::verify_agenda`'s
/// `SNIPPET_MAX_CHARS` (char count, not bytes — UTF-8 safe).
const SNIPPET_MAX_CHARS: usize = 280;

// ── §3 CONTRACT engine shapes (frozen) ──────────────────────────────────────

/// Tunables for one [`build_dream_agenda`] call.
#[derive(Clone, Copy, Debug)]
pub struct DreamConfig {
    pub limit: usize,
    pub min_cluster_size: usize,
    pub max_activation: Option<f64>,
    pub decay: f64,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            limit: 20,
            min_cluster_size: 1,
            max_activation: None,
            decay: lunaris_core::activation::DEFAULT_DECAY,
        }
    }
}

/// One candidate distillation cluster.
#[derive(Clone, Debug)]
pub struct DreamCluster {
    pub cluster_id: String,
    pub member_ids: Vec<Ulid>,
    pub mean_activation: f64,
    pub max_activation: f64,
    pub dominant_source: String,
    pub snippets: Vec<String>,
}

/// Output of one [`build_dream_agenda`] call.
#[derive(Clone, Debug)]
pub struct DreamAgenda {
    pub total_candidates: usize,
    pub clusters: Vec<DreamCluster>,
}

// ── internal ─────────────────────────────────────────────────────────────

/// One hydrated, filtered candidate episode.
struct Candidate {
    id: Ulid,
    source: String,
    content: String,
    activation: f64,
}

/// Minimal read-side mirror of the ad-hoc fact-row JSON `structured_ingest`
/// (in the `lunaris` umbrella crate) writes at
/// `lunaris_core::keyspace::fact_key`. `lunaris-consolidate` cannot depend on
/// `lunaris` (layering — `lunaris` depends on `lunaris-consolidate`), so this
/// is a narrow, non-`deny_unknown_fields` read of a producer-owned row (not a
/// validated request DTO — extra/unknown fields on the real row are simply
/// ignored).
#[derive(Deserialize)]
struct RawFact {
    #[serde(default)]
    subject_id: Option<[u8; 16]>,
    #[serde(default)]
    object_id: Option<[u8; 16]>,
    #[serde(default)]
    source_episode_id: Option<String>,
}

/// Validate a [`DreamConfig`] BEFORE any storage call. Returns the frozen
/// §3 CONTRACT reject code on failure.
fn validate(cfg: &DreamConfig) -> Result<(), &'static str> {
    if cfg.limit == 0 || cfg.limit > 100 {
        return Err("invalid_limit");
    }
    if cfg.min_cluster_size > 100 {
        return Err("invalid_min_cluster_size");
    }
    if cfg.max_activation.is_some_and(f64::is_nan) {
        return Err("invalid_max_activation");
    }
    Ok(())
}

/// Build a read-only distillation agenda for `scope`. See
/// `.add/tasks/dream-agenda/TASK.md` §3 CONTRACT (frozen) for the full
/// shape + behavior.
///
/// **READ-ONLY**: never calls `StoragePort::atomic_write`.
pub async fn build_dream_agenda(
    storage: Arc<dyn StoragePort>,
    scope: &Scope,
    cfg: &DreamConfig,
    now: u64,
) -> Result<DreamAgenda, LunarisError> {
    validate(cfg)
        .map_err(|code| LunarisError::Consolidate(ConsolError::Backend(code.to_string())))?;

    // MVCC "as of now" read point. The frozen §3 signature threads a plain
    // unix-seconds `now: u64` (no live `HlcClock` — the caller already
    // resolved wall-clock time before calling in). Derive the most-inclusive
    // Hlc within that second — the END of the millisecond range `now`
    // truncates to (`+999`), max counter/node_id — so every write committed
    // up to and including that second (regardless of its sub-second offset)
    // is visible. Mirrors the intent of `verify_agenda.rs`'s `clock.tick()`
    // read point without a clock handle.
    let read_at = Hlc::from_parts(now.saturating_mul(1000).saturating_add(999), u32::MAX, u16::MAX);

    // 1. Candidates = activation-ledger scan (task 2 primitive).
    let ledger = LedgerReferenceSource::new(storage.clone());
    let refs = ledger.scan(scope).await?;

    // 2. Hydrate + exclude + activation-filter.
    let mut candidates: Vec<Candidate> = Vec::with_capacity(refs.len());
    for (id, record) in refs {
        // engram-soul-loop task 8b (`memory.distill`) — an archived record
        // (activation drop via `ActivationRecord::archived_at`) is excluded
        // from the candidate set, same as a gone/distilled episode. This is
        // a cheap ledger-only check, so it runs BEFORE the episode hydrate
        // read below. The episode itself is untouched (not a tombstone) —
        // only its usage boost is suppressed here and in
        // `lunaris_retrieve::LedgerBoostProvider::priors`.
        if record.is_archived() {
            continue;
        }
        let key = episode_key(scope, id);
        let row =
            match storage.read_as_of(scope, &key, read_at).await.map_err(LunarisError::Storage)? {
                Some(row) => row,
                None => continue, // episode gone — excluded, never an error (§1 Must)
            };
        let episode = match serde_json::from_slice::<lunaris_core::Episode>(&row.value) {
            Ok(ep) => ep,
            Err(_) => continue, // corrupt row — treat as gone
        };
        if episode.source.starts_with("distilled:") {
            continue; // never re-distill a distilled record (§1 Must)
        }
        let activation = record.activation(now, cfg.decay);
        if cfg.max_activation.is_some_and(|ceiling| activation > ceiling) {
            continue; // not ripe (decayed) enough yet
        }
        candidates.push(Candidate {
            id,
            source: episode.source,
            content: episode.content,
            activation,
        });
    }

    let total_candidates = candidates.len();
    if candidates.is_empty() {
        return Ok(DreamAgenda { total_candidates, clusters: Vec::new() });
    }

    let candidate_ids: HashSet<Ulid> = candidates.iter().map(|c| c.id).collect();
    let by_id: HashMap<Ulid, &Candidate> = candidates.iter().map(|c| (c.id, c)).collect();

    // 3. Episode -> entity map, scoped to candidate-owned facts only.
    let episode_entities = scan_episode_entities(storage.as_ref(), scope, &candidate_ids).await?;

    // 4. Cluster: leiden path for episodes with entity signal, source-class
    //    fallback for the rest. Both paths always run; the leiden path is
    //    simply empty when no candidate has entity signal (§1 Must).
    let mut clusters_by_key: HashMap<String, Vec<Ulid>> = HashMap::new();

    if !episode_entities.is_empty() {
        let mut node_set: HashSet<EntityId> = HashSet::new();
        for ents in episode_entities.values() {
            node_set.extend(ents.iter().copied());
        }
        let mut nodes: Vec<EntityId> = node_set.into_iter().collect();
        nodes.sort_by_key(|e| e.0);

        let mut edges: Vec<(EntityId, EntityId)> = Vec::new();
        for ents in episode_entities.values() {
            for i in 0..ents.len() {
                for j in (i + 1)..ents.len() {
                    edges.push((ents[i], ents[j]));
                }
            }
        }

        let assignment = leiden_pass(&GraphSnapshot { nodes, edges }, MAX_LEIDEN_ITERS);

        for (episode_id, ents) in &episode_entities {
            // "First by EntityId byte order" — `ents` is already sorted
            // ascending by `.0` (see `scan_episode_entities`).
            let dominant = ents[0];
            let community = assignment
                .by_node
                .get(&dominant)
                .copied()
                .unwrap_or_else(|| CommunityId::from_members(&[dominant]));
            clusters_by_key.entry(format!("com:{community}")).or_default().push(*episode_id);
        }
    }

    for c in &candidates {
        if episode_entities.contains_key(&c.id) {
            continue; // already placed by the leiden path above
        }
        clusters_by_key.entry(format!("src:{}", source_class(&c.source))).or_default().push(c.id);
    }

    // 5. Materialize, drop under-min, sort (size DESC, mean_activation DESC), cap.
    let mut clusters: Vec<DreamCluster> = clusters_by_key
        .into_iter()
        .filter(|(_, members)| members.len() >= cfg.min_cluster_size)
        .map(|(cluster_id, mut members)| {
            members.sort();
            let activations: Vec<f64> = members.iter().map(|id| by_id[id].activation).collect();
            let mean_activation = activations.iter().sum::<f64>() / activations.len() as f64;
            let max_activation = activations.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let dominant_source = dominant_source_class(&members, &by_id);
            let snippets = members
                .iter()
                .take(3)
                .map(|id| truncate_snippet(&by_id[id].content, SNIPPET_MAX_CHARS))
                .collect();
            DreamCluster {
                cluster_id,
                member_ids: members,
                mean_activation,
                max_activation,
                dominant_source,
                snippets,
            }
        })
        .collect();

    clusters.sort_by(|a, b| {
        b.member_ids
            .len()
            .cmp(&a.member_ids.len())
            .then_with(|| b.mean_activation.total_cmp(&a.mean_activation))
    });
    clusters.truncate(cfg.limit);

    Ok(DreamAgenda { total_candidates, clusters })
}

/// Scan `fact_prefix(scope)` and build an episode -> sorted-entity-set map,
/// restricted to `candidate_ids` (facts owned by an excluded/non-candidate
/// episode never contribute a graph node). A malformed fact row is skipped,
/// never fatal (mirrors `LedgerReferenceSource::scan`'s skip-corrupt policy).
async fn scan_episode_entities(
    storage: &dyn StoragePort,
    scope: &Scope,
    candidate_ids: &HashSet<Ulid>,
) -> Result<HashMap<Ulid, Vec<EntityId>>, LunarisError> {
    let prefix = fact_prefix(scope);
    let mut stream =
        storage.scan_range(scope, &prefix, None).await.map_err(LunarisError::Storage)?;

    let mut raw: HashMap<Ulid, HashSet<EntityId>> = HashMap::new();
    while let Some(item) = stream.next().await {
        let (_key, value) = match item {
            Ok(kv) => kv,
            Err(e) => {
                tracing::warn!(err = %e, "dream_agenda_fact_scan_row_failed");
                continue;
            }
        };
        let Ok(fact) = serde_json::from_slice::<RawFact>(&value) else { continue };
        let Some(episode_id_str) = fact.source_episode_id else { continue };
        let Ok(episode_id) = Ulid::from_string(&episode_id_str) else { continue };
        if !candidate_ids.contains(&episode_id) {
            continue;
        }
        let entry = raw.entry(episode_id).or_default();
        if let Some(sid) = fact.subject_id {
            entry.insert(EntityId(sid));
        }
        if let Some(oid) = fact.object_id {
            entry.insert(EntityId(oid));
        }
    }

    Ok(raw
        .into_iter()
        .map(|(id, set)| {
            let mut v: Vec<EntityId> = set.into_iter().collect();
            v.sort_by_key(|e| e.0);
            (id, v)
        })
        .collect())
}

/// The source-class bucket: the `source` prefix before the first `:` (the
/// whole string when there is no `:`).
fn source_class(source: &str) -> String {
    match source.split_once(':') {
        Some((class, _)) => class.to_string(),
        None => source.to_string(),
    }
}

/// Most-common source-class among `members`, ties broken alphabetically
/// (deterministic — `BTreeMap` iterates ascending, first-seen wins ties).
fn dominant_source_class(members: &[Ulid], by_id: &HashMap<Ulid, &Candidate>) -> String {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for id in members {
        *counts.entry(source_class(&by_id[id].source)).or_insert(0) += 1;
    }
    let mut best: Option<(String, usize)> = None;
    for (class, count) in counts {
        best = match best {
            Some((bc, bcount)) if bcount >= count => Some((bc, bcount)),
            _ => Some((class, count)),
        };
    }
    best.map(|(c, _)| c).unwrap_or_default()
}

/// Truncate `content` to at most `max_chars` UTF-8 chars (never bytes).
/// Mirrors `lunaris-memory-service::verify_agenda::truncate_snippet`.
fn truncate_snippet(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        content.to_string()
    } else {
        content.chars().take(max_chars).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lunaris_core::activation::{ActivationRecord, Grain, RefSignal, Strength};
    use lunaris_core::keyspace::{activation_key, fact_key, scope_prefix};
    use lunaris_core::{Episode, HlcClock, WriteOp};
    use lunaris_test_harness::{TestStorage, open_test_storage};

    /// 0.7.0 port off `memory://` — a harness-issued backend (ephemeral
    /// child-process Moon, degrading to `memory://` where no Moon binary
    /// resolves). The `TestStorage` guard rides back with the port because it
    /// owns the Moon child; drop it and the backend dies mid-test.
    async fn fresh_storage() -> (Arc<dyn StoragePort>, TestStorage) {
        let storage = open_test_storage().await;
        (storage.port(), storage)
    }

    /// Real current unix seconds. `build_dream_agenda`'s `now` argument
    /// drives BOTH the activation-decay math AND the derived MVCC
    /// `read_as_of` visibility point (see the `read_at` doc comment above) —
    /// `EmbeddedStorage` stamps every write with a REAL wall-clock `Hlc`, so
    /// test fixtures must use a real epoch-seconds `now` (not a small
    /// fictional counter) or the derived `read_at` sits before the write and
    /// every row is invisible.
    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs()
    }

    fn scope() -> Scope {
        Scope::new(format!("test.dream-{}", Ulid::new())).unwrap()
    }

    fn episode(scope: &Scope, id: Ulid, source: &str, content: &str) -> Episode {
        let clock = HlcClock::new(0);
        let mut ep = Episode::new(scope.clone(), source, content, &clock);
        ep.id = id;
        ep
    }

    async fn put(storage: &Arc<dyn StoragePort>, scope: &Scope, key: Vec<u8>, value: Vec<u8>) {
        storage.atomic_write(scope, &[WriteOp::KvPut { key, value }]).await.expect("seed write");
    }

    async fn seed_episode(
        storage: &Arc<dyn StoragePort>,
        scope: &Scope,
        id: Ulid,
        source: &str,
        content: &str,
    ) {
        let ep = episode(scope, id, source, content);
        let value = serde_json::to_vec(&ep).unwrap();
        put(storage, scope, episode_key(scope, id), value).await;
    }

    async fn seed_activation(
        storage: &Arc<dyn StoragePort>,
        scope: &Scope,
        id: Ulid,
        refs: &[(u64, Strength)],
    ) {
        let mut record = ActivationRecord::default();
        for (at, strength) in refs {
            record.apply(&RefSignal { id, grain: Grain::Turn, strength: *strength }, *at);
        }
        let value = serde_json::to_vec(&record).unwrap();
        put(storage, scope, activation_key(scope, id), value).await;
    }

    fn fact_json(
        fact_id: Ulid,
        subject: EntityId,
        predicate: &str,
        object: EntityId,
        source_episode_id: Ulid,
        text: &str,
    ) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": fact_id.to_string(),
            "subject_id": subject.0,
            "predicate": predicate,
            "object_id": object.0,
            "fact_text": text,
            "confidence": 1.0,
            "valid_from_iso": "2026-01-01T00:00:00Z",
            "valid_to_iso": null,
            "source_episode_id": source_episode_id.to_string(),
        }))
        .unwrap()
    }

    async fn seed_fact(
        storage: &Arc<dyn StoragePort>,
        scope: &Scope,
        subject: EntityId,
        predicate: &str,
        object: EntityId,
        source_episode_id: Ulid,
        text: &str,
    ) {
        let fact_id = Ulid::new();
        let value = fact_json(fact_id, subject, predicate, object, source_episode_id, text);
        put(storage, scope, fact_key(scope, fact_id), value).await;
    }

    async fn key_count(storage: &Arc<dyn StoragePort>, scope: &Scope) -> usize {
        let prefix = scope_prefix(scope).into_bytes();
        let mut stream = storage.scan_range(scope, &prefix, None).await.unwrap();
        let mut n = 0usize;
        while stream.next().await.is_some() {
            n += 1;
        }
        n
    }

    /// §2 scenario: source-class grouping over referenced raw episodes
    /// (always-available path).
    #[tokio::test]
    async fn source_class_grouping_over_referenced_episodes() {
        let (storage, _storage_guard) = fresh_storage().await;
        let scope = scope();
        let base = unix_now();

        let mut lunaris_ids = Vec::new();
        for i in 0..3u8 {
            let id = Ulid::new();
            seed_episode(
                &storage,
                &scope,
                id,
                "lunaris:tool_call:post",
                &format!("lunaris note {i}"),
            )
            .await;
            seed_activation(&storage, &scope, id, &[(base - 10, Strength::Weak)]).await;
            lunaris_ids.push(id);
        }
        let mut edit_ids = Vec::new();
        for i in 0..2u8 {
            let id = Ulid::new();
            seed_episode(
                &storage,
                &scope,
                id,
                &format!("edit:{}", scope.as_str()),
                &format!("edit note {i}"),
            )
            .await;
            seed_activation(&storage, &scope, id, &[(base - 10, Strength::Weak)]).await;
            edit_ids.push(id);
        }

        // Captured AFTER every seed write (mirrors production sequencing —
        // `now` must be >= every prior write's real wall-clock stamp so the
        // derived `read_at` in `build_dream_agenda` can see them).
        let now = unix_now();
        let cfg = DreamConfig { limit: 20, min_cluster_size: 1, max_activation: None, decay: 0.5 };
        let agenda = build_dream_agenda(storage.clone(), &scope, &cfg, now).await.unwrap();

        assert_eq!(agenda.total_candidates, 5);
        assert_eq!(
            agenda.clusters.len(),
            2,
            "must produce exactly 2 source-class clusters: {:?}",
            agenda.clusters
        );

        let lunaris_cluster = agenda
            .clusters
            .iter()
            .find(|c| c.cluster_id == "src:lunaris")
            .expect("lunaris cluster present");
        assert_eq!(lunaris_cluster.member_ids.len(), 3);
        let mut expected = lunaris_ids.clone();
        expected.sort();
        assert_eq!(lunaris_cluster.member_ids, expected);
        assert!(!lunaris_cluster.snippets.is_empty() && lunaris_cluster.snippets.len() <= 3);
        assert_eq!(lunaris_cluster.dominant_source, "lunaris");

        let edit_cluster = agenda
            .clusters
            .iter()
            .find(|c| c.cluster_id == "src:edit")
            .expect("edit cluster present");
        assert_eq!(edit_cluster.member_ids.len(), 2);
        assert_eq!(edit_cluster.dominant_source, "edit");
    }

    /// §2 scenario: leiden entity-clustering when structured facts exist
    /// (no-LLM path) — proves `leiden_pass` gets a real call site.
    #[tokio::test]
    async fn leiden_entity_clustering_via_shared_fact_entity() {
        let (storage, _storage_guard) = fresh_storage().await;
        let scope = scope();
        let base = unix_now();

        let ep_a = Ulid::new();
        let ep_b = Ulid::new();
        let ep_c = Ulid::new();
        seed_episode(&storage, &scope, ep_a, "lunaris:a", "episode a").await;
        seed_episode(&storage, &scope, ep_b, "lunaris:b", "episode b").await;
        seed_episode(&storage, &scope, ep_c, "lunaris:c", "episode c").await;
        seed_activation(&storage, &scope, ep_a, &[(base - 10, Strength::Weak)]).await;
        seed_activation(&storage, &scope, ep_b, &[(base - 10, Strength::Weak)]).await;
        seed_activation(&storage, &scope, ep_c, &[(base - 10, Strength::Weak)]).await;

        let amber = EntityId::from_name_and_type("amber-relay", "concept");
        let foo_a = EntityId::from_name_and_type("foo-a", "concept");
        let foo_b = EntityId::from_name_and_type("foo-b", "concept");
        let zzz = EntityId::from_name_and_type("zzz-other", "concept");
        let qqq = EntityId::from_name_and_type("qqq-other2", "concept");

        seed_fact(&storage, &scope, amber, "relates_to", foo_a, ep_a, "amber relates to foo-a")
            .await;
        seed_fact(&storage, &scope, amber, "relates_to", foo_b, ep_b, "amber relates to foo-b")
            .await;
        seed_fact(&storage, &scope, zzz, "relates_to", qqq, ep_c, "zzz relates to qqq").await;

        let now = unix_now();
        let cfg = DreamConfig { limit: 20, min_cluster_size: 1, max_activation: None, decay: 0.5 };
        let agenda = build_dream_agenda(storage.clone(), &scope, &cfg, now).await.unwrap();

        assert_eq!(agenda.total_candidates, 3);
        let cluster_a = agenda
            .clusters
            .iter()
            .find(|c| c.member_ids.contains(&ep_a))
            .expect("a cluster containing ep_a must exist");
        assert!(
            cluster_a.cluster_id.starts_with("com:"),
            "leiden path must produce a com: cluster_id, got {}",
            cluster_a.cluster_id
        );
        assert!(
            cluster_a.member_ids.contains(&ep_b),
            "ep_a and ep_b must share the amber-relay community: {:?}",
            cluster_a.member_ids
        );
        assert!(
            !cluster_a.member_ids.contains(&ep_c),
            "ep_c shares no entity and must NOT be a member: {:?}",
            cluster_a.member_ids
        );
    }

    /// §2 scenario: distilled records are never candidates.
    #[tokio::test]
    async fn distilled_sources_are_excluded() {
        let (storage, _storage_guard) = fresh_storage().await;
        let scope = scope();
        let base = unix_now();

        let raw_id = Ulid::new();
        let distilled_id = Ulid::new();
        seed_episode(&storage, &scope, raw_id, "lunaris:tool_call:post", "raw episode").await;
        seed_episode(
            &storage,
            &scope,
            distilled_id,
            &format!("distilled:lesson:{}", scope.as_str()),
            "a distilled lesson",
        )
        .await;
        seed_activation(&storage, &scope, raw_id, &[(base - 10, Strength::Weak)]).await;
        seed_activation(&storage, &scope, distilled_id, &[(base - 10, Strength::Weak)]).await;

        let now = unix_now();
        let cfg = DreamConfig { limit: 20, min_cluster_size: 1, max_activation: None, decay: 0.5 };
        let agenda = build_dream_agenda(storage.clone(), &scope, &cfg, now).await.unwrap();

        assert_eq!(
            agenda.total_candidates, 1,
            "the distilled record must be excluded from candidates"
        );
        let all_members: Vec<Ulid> =
            agenda.clusters.iter().flat_map(|c| c.member_ids.clone()).collect();
        assert!(all_members.contains(&raw_id));
        assert!(!all_members.contains(&distilled_id));
    }

    /// §2 scenario: max_activation ceiling keeps only ripe (decayed) episodes.
    #[tokio::test]
    async fn max_activation_ceiling_keeps_only_decayed() {
        let (storage, _storage_guard) = fresh_storage().await;
        let scope = scope();
        let base = unix_now();

        let hot_id = Ulid::new();
        let cold_id = Ulid::new();
        seed_episode(&storage, &scope, hot_id, "lunaris:hot", "hot episode").await;
        seed_episode(&storage, &scope, cold_id, "lunaris:cold", "cold episode").await;
        // Hot: a strong ref one second before `base`.
        let hot_ref_at = base - 1;
        seed_activation(&storage, &scope, hot_id, &[(hot_ref_at, Strength::Strong)]).await;
        // Cold: a weak ref a very long time before `base`.
        let cold_ref_at = base.saturating_sub(999_999);
        seed_activation(&storage, &scope, cold_id, &[(cold_ref_at, Strength::Weak)]).await;

        // Captured AFTER every seed write — see the `source_class_grouping`
        // test's comment for why the read point must postdate the writes.
        let now = unix_now();
        let decay = 0.5;
        let hot_activation = {
            let mut r = ActivationRecord::default();
            r.apply(
                &RefSignal { id: hot_id, grain: Grain::Turn, strength: Strength::Strong },
                hot_ref_at,
            );
            r.activation(now, decay)
        };
        let cold_activation = {
            let mut r = ActivationRecord::default();
            r.apply(
                &RefSignal { id: cold_id, grain: Grain::Turn, strength: Strength::Weak },
                cold_ref_at,
            );
            r.activation(now, decay)
        };
        assert!(
            hot_activation > cold_activation,
            "fixture sanity: hot must score higher than cold"
        );

        let ceiling = cold_activation + 0.5; // just above the decayed value
        let cfg =
            DreamConfig { limit: 20, min_cluster_size: 1, max_activation: Some(ceiling), decay };
        let agenda = build_dream_agenda(storage.clone(), &scope, &cfg, now).await.unwrap();

        assert_eq!(agenda.total_candidates, 1, "only the decayed episode must pass the ceiling");
        let all_members: Vec<Ulid> =
            agenda.clusters.iter().flat_map(|c| c.member_ids.clone()).collect();
        assert!(all_members.contains(&cold_id));
        assert!(!all_members.contains(&hot_id));
    }

    /// §2 scenario: reject invalid limit/min_cluster_size/max_activation —
    /// none of these may touch storage: `PanicStorage` proves it (§1 Reject
    /// "no scan or read is performed against storage beyond validation").
    #[tokio::test]
    async fn reject_invalid_limit_min_cluster_size_and_max_activation() {
        struct PanicStorage;
        #[async_trait]
        impl StoragePort for PanicStorage {
            async fn atomic_write(
                &self,
                _scope: &Scope,
                _ops: &[WriteOp],
            ) -> Result<lunaris_core::Lsn, lunaris_core::StorageError> {
                panic!("validation must reject before any storage call");
            }
            async fn vector_search(
                &self,
                _scope: &Scope,
                _index: &str,
                _query: &[f32],
                _k: usize,
                _filter: Option<&lunaris_core::Filter>,
                _as_of: Option<Hlc>,
                _rerank: bool,
            ) -> Result<Vec<lunaris_core::VectorHit>, lunaris_core::StorageError> {
                panic!("validation must reject before any storage call");
            }
            async fn graph_traverse(
                &self,
                _scope: &Scope,
                _q: &lunaris_core::CypherQuery,
                _as_of: Option<Hlc>,
            ) -> Result<lunaris_core::GraphResult, lunaris_core::StorageError> {
                panic!("validation must reject before any storage call");
            }
            async fn scan_range(
                &self,
                _scope: &Scope,
                _prefix: &[u8],
                _as_of: Option<Hlc>,
            ) -> Result<
                futures::stream::BoxStream<
                    '_,
                    Result<(bytes::Bytes, bytes::Bytes), lunaris_core::StorageError>,
                >,
                lunaris_core::StorageError,
            > {
                panic!("validation must reject before any storage call");
            }
            async fn read_as_of(
                &self,
                _scope: &Scope,
                _key: &[u8],
                _as_of: Hlc,
            ) -> Result<Option<lunaris_core::Row<bytes::Bytes>>, lunaris_core::StorageError>
            {
                panic!("validation must reject before any storage call");
            }
            async fn publish(
                &self,
                _scope: &Scope,
                _topic: &str,
                _partition: u16,
                _payload: bytes::Bytes,
            ) -> Result<u64, lunaris_core::StorageError> {
                panic!("validation must reject before any storage call");
            }
            async fn subscribe(
                &self,
                _scope: &Scope,
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
                panic!("validation must reject before any storage call");
            }
            fn capabilities(&self) -> lunaris_core::StorageCapabilities {
                panic!("validation must reject before any storage call");
            }
        }

        let storage: Arc<dyn StoragePort> = Arc::new(PanicStorage);
        let scope = scope();

        let limit_zero =
            DreamConfig { limit: 0, min_cluster_size: 1, max_activation: None, decay: 0.5 };
        let err =
            build_dream_agenda(storage.clone(), &scope, &limit_zero, 1_000).await.unwrap_err();
        assert!(err.to_string().contains("invalid_limit"), "{err}");

        let limit_over =
            DreamConfig { limit: 101, min_cluster_size: 1, max_activation: None, decay: 0.5 };
        let err =
            build_dream_agenda(storage.clone(), &scope, &limit_over, 1_000).await.unwrap_err();
        assert!(err.to_string().contains("invalid_limit"), "{err}");

        let bad_min =
            DreamConfig { limit: 20, min_cluster_size: 101, max_activation: None, decay: 0.5 };
        let err = build_dream_agenda(storage.clone(), &scope, &bad_min, 1_000).await.unwrap_err();
        assert!(err.to_string().contains("invalid_min_cluster_size"), "{err}");

        let bad_max = DreamConfig {
            limit: 20,
            min_cluster_size: 1,
            max_activation: Some(f64::NAN),
            decay: 0.5,
        };
        let err = build_dream_agenda(storage.clone(), &scope, &bad_max, 1_000).await.unwrap_err();
        assert!(err.to_string().contains("invalid_max_activation"), "{err}");
    }

    /// engram-soul-loop task 8b (`memory.distill`) — an ARCHIVED source
    /// (`ActivationRecord::archived_at` set) must drop out of the candidate
    /// set entirely, same as a `distilled:*` source or a gone episode. A
    /// live sibling with an identical shape must still be a candidate — the
    /// exclusion is per-record, not scope-wide.
    #[tokio::test]
    async fn archived_sources_are_excluded_from_candidates() {
        let (storage, _storage_guard) = fresh_storage().await;
        let scope = scope();
        let base = unix_now();

        let live_id = Ulid::new();
        let archived_id = Ulid::new();
        seed_episode(&storage, &scope, live_id, "lunaris:tool_call:post", "live episode").await;
        seed_episode(&storage, &scope, archived_id, "lunaris:tool_call:post", "archived episode")
            .await;
        seed_activation(&storage, &scope, live_id, &[(base - 10, Strength::Weak)]).await;

        // Archived: apply a ref, then stamp archived_at directly (mirrors
        // `ScopedLunaris::archive_activation`'s RMW — this test seeds the
        // ledger row shape by hand, same as `seed_activation`).
        let mut archived_record = ActivationRecord::default();
        archived_record.apply(
            &RefSignal { id: archived_id, grain: Grain::Turn, strength: Strength::Weak },
            base - 10,
        );
        archived_record.archived_at = Some(base);
        assert!(archived_record.is_archived());
        put(
            &storage,
            &scope,
            activation_key(&scope, archived_id),
            serde_json::to_vec(&archived_record).unwrap(),
        )
        .await;

        let now = unix_now();
        let cfg = DreamConfig { limit: 20, min_cluster_size: 1, max_activation: None, decay: 0.5 };
        let agenda = build_dream_agenda(storage.clone(), &scope, &cfg, now).await.unwrap();

        assert_eq!(
            agenda.total_candidates, 1,
            "the archived source must be excluded from candidates: {:?}",
            agenda.clusters
        );
        let all_members: Vec<Ulid> =
            agenda.clusters.iter().flat_map(|c| c.member_ids.clone()).collect();
        assert!(all_members.contains(&live_id), "the live sibling must still be a candidate");
        assert!(
            !all_members.contains(&archived_id),
            "the archived source must never appear in a cluster: {:?}",
            agenda.clusters
        );
    }

    /// §6 VERIFY: a dream_agenda call writes nothing — storage key-count
    /// snapshot before/after is byte-identical.
    #[tokio::test]
    async fn build_dream_agenda_writes_nothing() {
        let (storage, _storage_guard) = fresh_storage().await;
        let scope = scope();

        let base = unix_now();
        let id = Ulid::new();
        seed_episode(&storage, &scope, id, "lunaris:x", "some note").await;
        seed_activation(&storage, &scope, id, &[(base - 10, Strength::Weak)]).await;

        let now = unix_now();
        let before = key_count(&storage, &scope).await;
        let cfg = DreamConfig::default();
        let agenda = build_dream_agenda(storage.clone(), &scope, &cfg, now).await.unwrap();
        assert_eq!(
            agenda.total_candidates, 1,
            "fixture sanity: the seeded episode must be a candidate"
        );
        let after = key_count(&storage, &scope).await;

        assert_eq!(before, after, "build_dream_agenda must write nothing");
    }
}
