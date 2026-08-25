//! Plan 05-02 — deterministic fixture corpus for `lunaris-conformance::storage`.
//!
//! 10 episodes seeded from a fixed `0xCAFE_F00D` ChaCha20 RNG so re-runs
//! across independent stores produce byte-identical primitive bytes.
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
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use ulid::Ulid;

/// Vector dimensionality used by fixture-seeded embeddings. Must match
/// `MoonClient::ensure_indexes` (`const DIM: usize = 768`). Keeping the constant here
/// (instead of hard-coding 768 at every call site) localises the update when
/// the canonical dimension moves.
pub const EMBED_DIM: usize = 768;

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
    /// the same `SEED`, so calling this against two independent stores
    /// produces byte-identical primitive bytes.
    ///
    /// Each episode lands as a SINGLE `atomic_write` carrying BOTH the
    /// `WriteOp::KvPut` raw episode payload AND a `WriteOp::VectorUpsert`
    /// into the `chunks` index so the `vector_search` parity suite has
    /// searchable rows (INGEST-04 single-call invariant
    /// per Episode preserved — one `atomic_write` call per episode).
    pub async fn ingest_into(&self, storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
        for ep in &self.episodes {
            let ops = build_episode_ops(ep)?;
            storage.atomic_write(&lunaris_core::Scope::dev(), &ops).await?;
        }
        Ok(())
    }
}

fn build_ten_episodes(seed: u64) -> Vec<Episode> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    (0..EPISODE_COUNT)
        .map(|i| {
            let id = ulid_from_rng(&mut rng);
            let stamp = Hlc { wall_ms: 1_000_000 + (i as u64) * 1_000, counter: 0, node_id: 0 };
            Episode {
                id,
                scope: lunaris_core::Scope::dev(), // RFC 0001 Wave 0 migration crutch
                source: format!("conformance:fixture/{i}"),
                content: lorem_for(i),
                t_ref: None,
                bt: BiTemporal { valid: (stamp, None), sys: (stamp, None) },
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
        ("beta".to_string(), Some(Hlc { wall_ms: 1_002_000, counter: 0, node_id: 0 })),
        // Roughly mid-corpus.
        ("gamma".to_string(), Some(Hlc { wall_ms: 1_005_000, counter: 0, node_id: 0 })),
        // Past the last episode — semantically "latest".
        ("delta".to_string(), Some(Hlc { wall_ms: 1_009_000, counter: 0, node_id: 0 })),
        // Before any episode — empty result expected.
        ("epsilon".to_string(), Some(Hlc { wall_ms: 1, counter: 0, node_id: 0 })),
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
    // Seed the `chunks` FT / pgvector index so `vector_search("chunks", ...)`
    // has rows on both backends. Embedding is derived from the episode
    // content via the same `stub_embed` the parity query path uses, so query
    // and stored vectors live in the same metric space.
    //
    // Metadata carries:
    //   * `text`     — picked up by Moon's `extract_content_for_index` for
    //                  BM25 payloads.
    //   * `source`   — chunks FT index has a `SchemaField::Tag("source")`
    //                  declared; keeps parity with production writes.
    //   * `valid_time_ms` — chunks FT index has `SchemaField::Numeric("valid_time")`;
    //                  populated from the episode's bitemporal valid stamp so
    //                  `Filter::ValidTimeRange` queries can resolve.
    let embedding = stub_embed(&ep.content);
    let metadata = serde_json::json!({
        "text": ep.content,
        "source": ep.source,
        "valid_time_ms": ep.bt.valid.0.wall_ms,
    });
    Ok(vec![
        WriteOp::KvPut { key, value },
        WriteOp::VectorUpsert {
            index: "chunks".to_string(),
            id: ep.id.to_bytes().to_vec(),
            embedding,
            metadata,
        },
    ])
}

/// Deterministic 768-d stub embedder — [`lunaris_core::det_vec`] itself, so
/// fixture-stored embeddings sit in exactly the metric space production
/// `StubEmbedder(768)` produces.
///
/// This was the third hand-copy of that algorithm in the workspace (core,
/// lunaris-bench, here), each with a doc claiming to mirror the canonical one
/// and each carrying its bug: a `>> 33` that left every "uniform [-1, 1]"
/// coordinate in [-1, 0]. `stub_embed_matches_canonical_det_vec_algorithm`
/// compared this copy against core and passed throughout, because two copies
/// of the same mistake agree perfectly. Matching is not the same as being
/// right, so the copy is gone and the test now guards against a new one.
///
/// Exposed at `pub(crate)` so both the fixture ingest path AND the parity
/// query path in `storage::as_of_parity` compute matching vectors for the
/// same input string.
pub(crate) fn stub_embed(s: &str) -> Vec<f32> {
    lunaris_core::det_vec(s, EMBED_DIM)
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
    run: &crate::suite_scope::SuiteScope,
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
    // F11 — the run's own scope, not `Scope::dev()`. The chunk IDs below stay
    // fixed on purpose: Moon keys a vector at `{ft_index_name(scope, kind)}:{id}`,
    // so the scope alone partitions them. (KV is the leg that does NOT get that
    // for free — see `SuiteScope::key`.)
    storage.atomic_write(run.scope(), &ops).await?;
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
pub async fn seed_one_edge(
    storage: &Arc<dyn StoragePort>,
    run: &crate::suite_scope::SuiteScope,
) -> anyhow::Result<()> {
    let alice_id = b"conformance_alice".to_vec();
    let bob_id = b"conformance_bob".to_vec();

    // Node IDs stay fixed: Moon writes graph ops into `graph_key(scope)`, so
    // the run's scope is what keeps two invocations off each other's edges.
    storage
        .atomic_write(
            run.scope(),
            &[
                WriteOp::GraphNode {
                    graph: "lunaris_graph".to_string(),
                    id: alice_id.clone(),
                    label: "Person".to_string(),
                    props: serde_json::json!({ "name": "Alice" }),
                    index_kind: "entities".to_string(),
                },
                WriteOp::GraphNode {
                    graph: "lunaris_graph".to_string(),
                    id: bob_id.clone(),
                    label: "Person".to_string(),
                    props: serde_json::json!({ "name": "Bob" }),
                    index_kind: "entities".to_string(),
                },
                WriteOp::GraphEdge {
                    graph: "lunaris_graph".to_string(),
                    src: alice_id,
                    dst: bob_id,
                    rel: "KNOWS".to_string(),
                    props: serde_json::json!({}),
                },
            ],
        )
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
            // One KvPut (raw episode payload) + one VectorUpsert (chunks index seed).
            assert_eq!(ops.len(), 2);
            match &ops[0] {
                WriteOp::KvPut { key, value } => {
                    assert!(key.starts_with(b"episode:conformance:"));
                    let back: Episode = serde_json::from_slice(value).expect("episode round trip");
                    assert_eq!(back.id, ep.id);
                }
                other => panic!("expected KvPut at [0], got {other:?}"),
            }
            match &ops[1] {
                WriteOp::VectorUpsert { index, id, embedding, metadata } => {
                    assert_eq!(index, "chunks");
                    assert_eq!(id, &ep.id.to_bytes().to_vec());
                    assert_eq!(embedding.len(), EMBED_DIM, "embedding must be 768d");
                    assert_eq!(
                        metadata.get("text").and_then(|v| v.as_str()),
                        Some(ep.content.as_str())
                    );
                    assert_eq!(
                        metadata.get("source").and_then(|v| v.as_str()),
                        Some(ep.source.as_str())
                    );
                    assert_eq!(
                        metadata.get("valid_time_ms").and_then(|v| v.as_u64()),
                        Some(ep.bt.valid.0.wall_ms),
                    );
                }
                other => panic!("expected VectorUpsert at [1], got {other:?}"),
            }
        }
    }

    #[test]
    fn stub_embed_is_deterministic_and_768d() {
        let a = stub_embed("alpha");
        let b = stub_embed("alpha");
        assert_eq!(a, b);
        assert_eq!(a.len(), EMBED_DIM);
    }

    #[test]
    fn stub_embed_distinguishes_inputs() {
        let a = stub_embed("alpha");
        let b = stub_embed("beta");
        assert_ne!(a, b, "different inputs MUST produce different vectors");
    }

    #[test]
    fn stub_embed_matches_canonical_det_vec_algorithm() {
        use lunaris_core::embedder::{Embedder, StubEmbedder};
        let canonical = StubEmbedder::new(EMBED_DIM);
        let got = stub_embed("parity-check");
        let expected = futures::executor::block_on(canonical.embed_batch(&["parity-check"]))
            .expect("stub embedder never fails");
        assert_eq!(
            got, expected[0],
            "fixture stub_embed must match canonical det_vec byte-for-byte"
        );
    }
}
