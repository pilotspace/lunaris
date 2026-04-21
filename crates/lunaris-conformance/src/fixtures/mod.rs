//! Plan 05-02 — deterministic fixture corpus for `lunaris-conformance::storage`.
//!
//! 10 episodes seeded from a fixed `0xCAFE_F00D` ChaCha20 RNG so re-runs
//! across Moon and Postgres produce byte-identical primitive bytes.
//! Mirrors the determinism contract from `crates/lunaris-bench/src/corpus.rs`
//! lines 1-150 (Shared Pattern 5 in PATTERNS.md).
//!
//! Also exposes two helper seeders used by the storage suite:
//!
//! - [`seed_three_chunks`] — atomically writes 3 chunk vector primitives
//!   whose embeddings are predictable: `target_vec` lands at index 0; the
//!   other two are perturbed copies. Lets `vector_search::recall` make a
//!   deterministic top-1 assertion.
//! - [`seed_one_edge`] — atomically writes 2 entity nodes + 1 relation
//!   edge so a Cypher `MATCH (n)-[r]->(m) RETURN n` returns at least one
//!   row.
//!
//! Both helpers preserve the **single `atomic_write` invariant** (INGEST-04
//! per `02-CONTEXT.md` and Plan 03-03 / 04-04 / 04-05 reaffirmations):
//! a helper that ships several primitives ships them all in ONE
//! `atomic_write` call.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::hlc::Hlc;
use lunaris_core::primitives::Episode;
use lunaris_core::storage::types::WriteOp;
use lunaris_core::storage::StoragePort;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use ulid::Ulid;

/// Deterministic ChaCha20 seed for the conformance fixture corpus. Distinct
/// from `crates/lunaris-bench/src/corpus.rs::DEFAULT_CORPUS_SEED` (`42`) so a
/// bench corpus and a conformance corpus never accidentally share IDs.
pub const SEED: u64 = 0xCAFE_F00D;

/// Number of episodes the fixture corpus emits. Locked at 10 per CONTEXT.md
/// D-12 ("ingests a fixed 10-episode corpus into both backends").
pub const EPISODE_COUNT: usize = 10;

/// 10-episode fixture corpus + a small `(query, as_of)` set spanning t0/t1/t2/
/// latest/before-all so the AS_OF parity test (STORE-07) exercises every
/// temporal regime.
pub struct FixtureCorpus {
    episodes: Vec<Episode>,
    queries: Vec<(String, Option<Hlc>)>,
}

impl Default for FixtureCorpus {
    fn default() -> Self {
        Self::new()
    }
}

impl FixtureCorpus {
    /// Build the deterministic 10-episode corpus + the 5-tuple query set.
    pub fn new() -> Self {
        let episodes = build_ten_episodes(SEED);
        let queries = build_query_set();
        Self { episodes, queries }
    }

    /// All 10 fixture episodes in deterministic order.
    pub fn episodes(&self) -> &[Episode] {
        &self.episodes
    }

    /// 5 `(query, as_of)` tuples spanning t0 / t1 / t2 / latest / before-all.
    pub fn query_set(&self) -> &[(String, Option<Hlc>)] {
        &self.queries
    }

    /// Atomic-write every episode into the given backend. Deterministic on
    /// the same `SEED`, so calling this on Moon and Postgres produces
    /// byte-identical primitive bytes.
    ///
    /// Each episode lands as a SINGLE `atomic_write` of one
    /// `WriteOp::KvPut` (INGEST-04 single-call invariant per Episode).
    pub async fn ingest_into(&self, storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
        for ep in &self.episodes {
            let ops = build_episode_ops(ep)?;
            storage.atomic_write(&ops).await?;
        }
        Ok(())
    }
}

fn build_ten_episodes(seed: u64) -> Vec<Episode> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    (0..EPISODE_COUNT)
        .map(|i| {
            let id = ulid_from_rng(&mut rng);
            let stamp = Hlc {
                wall_ms: 1_000_000 + (i as u64) * 1_000,
                counter: 0,
                node_id: 0,
            };
            Episode {
                id,
                source: format!("conformance:fixture/{i}"),
                content: lorem_for(i),
                t_ref: None,
                bt: BiTemporal {
                    valid: (stamp, None),
                    sys: (stamp, None),
                },
                metadata: serde_json::Map::new(),
            }
        })
        .collect()
}

fn build_query_set() -> Vec<(String, Option<Hlc>)> {
    vec![
        // No as_of — read latest.
        ("alpha".to_string(), None),
        // Just after the second episode lands.
        (
            "beta".to_string(),
            Some(Hlc { wall_ms: 1_002_000, counter: 0, node_id: 0 }),
        ),
        // Roughly mid-corpus.
        (
            "gamma".to_string(),
            Some(Hlc { wall_ms: 1_005_000, counter: 0, node_id: 0 }),
        ),
        // Past the last episode — semantically "latest".
        (
            "delta".to_string(),
            Some(Hlc { wall_ms: 1_009_000, counter: 0, node_id: 0 }),
        ),
        // Before any episode — empty result expected.
        (
            "epsilon".to_string(),
            Some(Hlc { wall_ms: 1, counter: 0, node_id: 0 }),
        ),
    ]
}

fn ulid_from_rng(rng: &mut ChaCha20Rng) -> Ulid {
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes);
    Ulid::from_bytes(bytes)
}

fn lorem_for(i: usize) -> String {
    // Deterministic lorem-ipsum-style content seeded by index. Real fixture
    // content stays small (~200 chars) — these are conformance fixtures, not
    // perf fixtures.
    let filler = "the quick brown fox jumps over the lazy dog ".repeat(2 + i % 3);
    format!(
        "Episode {i}: alpha beta gamma delta epsilon. Repeated context for vector recall: {filler}."
    )
}

fn build_episode_ops(ep: &Episode) -> anyhow::Result<Vec<WriteOp>> {
    let key = format!("episode:conformance:{}", ep.id).into_bytes();
    let value = serde_json::to_vec(ep)?;
    Ok(vec![WriteOp::KvPut { key, value }])
}

// ---------------------------------------------------------------------------
// B-3 fix per Plan 05-02 frontmatter: helper seeders used by Task 2
// (vector_search::recall + graph_traverse::cypher_subset). Defined HERE in
// Task 1 so Task 2 finds them already present (no forward refs). Each helper
// writes via ONE atomic_write to preserve the single-call invariant.
// ---------------------------------------------------------------------------

/// Atomically writes 3 `WriteOp::VectorUpsert` primitives into the `chunks`
/// vector index whose embeddings are predictable:
///
/// * Index 0 stores `target_vec` verbatim.
/// * Indices 1 and 2 store perturbed copies (`+0.01`, `+0.02` per component)
///   so a `vector_search` query equal to `target_vec` ranks index-0 first.
///
/// Returns the number of vectors written so the caller can assert the
/// pre-condition before issuing the search.
///
/// Single `atomic_write` per INGEST-04 invariant.
pub async fn seed_three_chunks(
    storage: &Arc<dyn StoragePort>,
    target_vec: &[f32],
) -> anyhow::Result<usize> {
    let mut ops: Vec<WriteOp> = Vec::with_capacity(3);
    for i in 0..3u32 {
        let id = format!("chunk-conformance-{i:03}").into_bytes();
        let mut emb: Vec<f32> = target_vec.to_vec();
        if i > 0 {
            let delta = 0.01_f32 * (i as f32);
            for v in emb.iter_mut() {
                *v += delta;
            }
        }
        let metadata = serde_json::json!({
            "source": format!("conformance:vector_search/{i}"),
        });
        ops.push(WriteOp::VectorUpsert {
            index: "chunks".to_string(),
            id,
            embedding: emb,
            metadata,
        });
    }
    let count = ops.len();
    storage.atomic_write(&ops).await?;
    Ok(count)
}

/// Atomically writes 2 `WriteOp::GraphNode` + 1 `WriteOp::GraphEdge`
/// primitives into the `lunaris_graph` graph store so a Cypher
/// `MATCH (n)-[r]->(m) RETURN n` returns at least one row.
///
/// Node/edge IDs and labels match the validated regex
/// `^[A-Za-z_][A-Za-z0-9_]*$` per the T-01-03-01 / T-01-04-03 contracts.
///
/// Single `atomic_write` per INGEST-04 invariant.
pub async fn seed_one_edge(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let alice_id = b"conformance_alice".to_vec();
    let bob_id = b"conformance_bob".to_vec();

    storage
        .atomic_write(&[
            WriteOp::GraphNode {
                graph: "lunaris_graph".to_string(),
                id: alice_id.clone(),
                label: "Person".to_string(),
                props: serde_json::json!({ "name": "Alice" }),
            },
            WriteOp::GraphNode {
                graph: "lunaris_graph".to_string(),
                id: bob_id.clone(),
                label: "Person".to_string(),
                props: serde_json::json!({ "name": "Bob" }),
            },
            WriteOp::GraphEdge {
                graph: "lunaris_graph".to_string(),
                src: alice_id,
                dst: bob_id,
                rel: "KNOWS".to_string(),
                props: serde_json::json!({}),
            },
        ])
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_episodes_are_deterministic() {
        let a = FixtureCorpus::new();
        let b = FixtureCorpus::new();
        assert_eq!(a.episodes().len(), EPISODE_COUNT);
        assert_eq!(b.episodes().len(), EPISODE_COUNT);
        for (a_ep, b_ep) in a.episodes().iter().zip(b.episodes().iter()) {
            assert_eq!(a_ep.id, b_ep.id, "ulid drift across runs");
            assert_eq!(a_ep.source, b_ep.source);
            assert_eq!(a_ep.content, b_ep.content);
            assert_eq!(a_ep.bt, b_ep.bt);
        }
    }

    #[test]
    fn query_set_has_five_entries() {
        let c = FixtureCorpus::new();
        assert_eq!(c.query_set().len(), 5);
        // Sanity: at least one query has Some(as_of).
        assert!(c.query_set().iter().any(|(_, a)| a.is_some()));
        // Sanity: at least one has None (latest).
        assert!(c.query_set().iter().any(|(_, a)| a.is_none()));
    }

    #[test]
    fn episode_ops_round_trip_via_serde() {
        let c = FixtureCorpus::new();
        for ep in c.episodes() {
            let ops = build_episode_ops(ep).expect("episode ops build");
            assert_eq!(ops.len(), 1);
            match &ops[0] {
                WriteOp::KvPut { key, value } => {
                    assert!(key.starts_with(b"episode:conformance:"));
                    let back: Episode =
                        serde_json::from_slice(value).expect("episode round trip");
                    assert_eq!(back.id, ep.id);
                }
                other => panic!("expected KvPut, got {other:?}"),
            }
        }
    }
}
