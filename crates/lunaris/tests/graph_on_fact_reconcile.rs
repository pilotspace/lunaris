//! RED — Δ4 memory-update convergence on the LLM-EXTRACTION path.
//!
//! `structured_ingest` (agent-supplied facts) has carried the full
//! memory-update machinery since the mem0-parity wave: deterministic
//! `FactId` for sync dedup, an `fact_spo_key` secondary index, and
//! `classify_fact` → `NeedsReviewItem::CrossEpisodeContradiction` routed to
//! the async verify queue. `ingest_episode_graph_on` — the path the LLM
//! extractor (and therefore LongMemEval) actually uses — has NONE of it:
//!
//! 1. **Random fact ids.** `llm_extractor::into_raw` mints `Ulid::new()` per
//!    fact, so the SAME (subject, predicate, object) re-asserted in a later
//!    session lands as a SECOND row. The validator's Wave-D dedup is
//!    within-batch only AND keys on `valid_from`, so with per-session date
//!    grounding cross-session re-assertions never collapse. Duplicate rows
//!    burn rerank-pool slots (feeding the displacement regression) and
//!    render as repeated reader bullets — both observed in the 2026-07-29
//!    cache audit.
//! 2. **No spo index.** graph-ON writes no `fact_spo_key` rows at all, so
//!    cross-episode contradictions are never detected and even a running
//!    verifier would have nothing to arbitrate.
//!
//! Contract pinned here — graph-ON reaches parity with structured ingest:
//! deterministic ids, spo-index writes folded into the SAME `ops` vec
//! (INGEST-04: still exactly ONE `atomic_write` per ingest), and
//! contradictions published to `__lunaris_verify__` for ASYNC arbitration
//! (blueprint §3.2 subsystem 5 — arbitration is explicitly off the hot
//! path, so this does NOT close `valid_to` synchronously).
//!
//! Facts whose `valid_from` resolves from neither the extraction nor the
//! episode's `t_ref` are skipped by reconciliation (append-only, no spo
//! entry) — fail-open, never a false supersede.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::TimeZone;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Extractor, Lunaris};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, keyspace::fact_spo_key,
};
use lunaris_extract::types::EntityId;
use lunaris_extract::{
    ChunkInput, Fact as ExtractFact, RawExtraction, RawExtractionBatch, types::FactId,
};
use parking_lot::Mutex;
use ulid::Ulid;

const DIM: usize = 768;

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|_| vec![0.1f32; DIM]).collect())
    }
}

/// Storage that PERSISTS KvPuts (so a later ingest's spo read sees an
/// earlier ingest's index) and records every batch + publish.
#[derive(Default)]
struct RecordingStorage {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
    published: Mutex<Vec<(String, serde_json::Value)>>,
}

impl RecordingStorage {
    fn kv_keys_matching(&self, needle: &str) -> Vec<String> {
        self.rows
            .lock()
            .keys()
            .filter_map(|k| String::from_utf8(k.clone()).ok())
            .filter(|s| s.contains(needle))
            .collect()
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            if let WriteOp::KvPut { key, value } = op {
                self.rows.lock().insert(key.clone(), value.clone());
            }
        }
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }
    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }
    async fn publish(
        &self,
        _s: &lunaris_core::Scope,
        topic: &str,
        _p: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        let v = serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null);
        self.published.lock().push((topic.to_owned(), v));
        Ok(0)
    }
    async fn subscribe(
        &self,
        _s: &lunaris_core::Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            // `publish` below is a real recorder, so the capability must claim
            // the queue — `publish_needs_review` skips publishing entirely when
            // `queue_native` is false, which would make the contradiction test
            // unwinnable regardless of the production path.
            queue_native: true,
            max_vector_dim: DIM as u32,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorage {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

const SUBJ: &str = "user";
const SUBJ_TY: &str = "Person";
const PRED: &str = "prefers";

fn eid(name: &str, ty: &str) -> EntityId {
    EntityId::from_name_and_type(name, ty)
}

/// Extractor emitting ONE fact whose object + window come from a swappable
/// slot, so successive ingests can assert different objects.
#[derive(Default)]
struct ScriptedExtractor {
    next: Mutex<Option<(String, String)>>, // (object_name, valid_from_iso)
}

impl ScriptedExtractor {
    fn set(&self, object: &str, valid_from_iso: &str) {
        *self.next.lock() = Some((object.to_owned(), valid_from_iso.to_owned()));
    }
}

#[async_trait]
impl Extractor for ScriptedExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        let (object, vf) = self.next.lock().clone().expect("script set before ingest");
        let by_chunk = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let facts = if i == 0 {
                    vec![ExtractFact {
                        // Random id ON PURPOSE: the production extractor mints
                        // Ulid::new() today. The ingest path must impose the
                        // DETERMINISTIC identity regardless of what the
                        // extractor supplied, or dedup depends on extractor
                        // goodwill.
                        id: Ulid::new(),
                        subject_id: eid(SUBJ, SUBJ_TY),
                        predicate: PRED.into(),
                        object_id: eid(&object, "Thing"),
                        fact_text: format!("The user prefers {object}"),
                        confidence: 0.9,
                        valid_from_iso: vf.clone(),
                        valid_to_iso: None,
                    }]
                } else {
                    vec![]
                };
                RawExtraction {
                    source_chunk_id: c.chunk_id,
                    entities: vec![],
                    relations: vec![],
                    facts,
                }
            })
            .collect();
        Ok(RawExtractionBatch { by_chunk })
    }
    fn applies(&self) -> bool {
        true
    }
}

fn make(
    rec: Arc<RecordingStorage>,
    ex: Arc<ScriptedExtractor>,
    clock: Arc<HlcClock>,
) -> Arc<Lunaris> {
    let h = Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder) as Arc<dyn Embedder>,
        clock,
    );
    h.graph_pipeline().enable();
    h.graph_pipeline().set_extractor(ex);
    Arc::new(h)
}

async fn ingest_one(
    h: &Lunaris,
    clock: &HlcClock,
    source: &str,
    body: &str,
    date: (i32, u32, u32),
) {
    let mut ep = Episode::new(lunaris_core::Scope::dev(), source, body, clock);
    ep.t_ref = Some(chrono::Utc.with_ymd_and_hms(date.0, date.1, date.2, 0, 0, 0).unwrap());
    h.ingest(ep).await.expect("graph-ON ingest must succeed");
}

/// The fact row key must be the DETERMINISTIC FactId of the triple, so the
/// same assertion in a later session overwrites in place instead of
/// accruing a duplicate row.
#[tokio::test]
async fn identical_fact_across_episodes_collapses_to_one_row() {
    let rec = Arc::new(RecordingStorage::default());
    let ex = Arc::new(ScriptedExtractor::default());
    let clock = HlcClock::new(0);
    let h = make(rec.clone(), ex.clone(), clock.clone());

    let expected_id =
        Ulid::from_bytes(FactId::from_triple(eid(SUBJ, SUBJ_TY), PRED, eid("tea", "Thing")).0);

    ex.set("tea", "2023-05-01");
    ingest_one(&h, &clock, "s1.md", "The user prefers tea.", (2023, 5, 1)).await;
    ex.set("tea", "2023-06-01"); // same triple, later session
    ingest_one(&h, &clock, "s2.md", "The user still prefers tea.", (2023, 6, 1)).await;

    let fact_keys = rec.kv_keys_matching(":fact:");
    assert_eq!(
        fact_keys.len(),
        1,
        "same triple re-asserted must collapse to ONE fact row; got {fact_keys:?}"
    );
    assert!(
        fact_keys[0].ends_with(&expected_id.to_string()),
        "fact row must be keyed by the deterministic FactId ({expected_id}); got {}",
        fact_keys[0]
    );

    // Re-assertion is a Noop, never a contradiction.
    let verify_msgs: Vec<_> =
        rec.published.lock().iter().filter(|(t, _)| t == "__lunaris_verify__").cloned().collect();
    assert!(
        verify_msgs.is_empty(),
        "re-asserting the same object must NOT flag a contradiction; got {verify_msgs:?}"
    );
}

/// graph-ON must write the `(subject, predicate)` spo index — without it no
/// contradiction can ever be detected on the extraction path.
#[tokio::test]
async fn graph_on_ingest_writes_spo_index() {
    let rec = Arc::new(RecordingStorage::default());
    let ex = Arc::new(ScriptedExtractor::default());
    let clock = HlcClock::new(0);
    let h = make(rec.clone(), ex.clone(), clock.clone());

    ex.set("tea", "2023-05-01");
    ingest_one(&h, &clock, "s1.md", "The user prefers tea.", (2023, 5, 1)).await;

    let want =
        String::from_utf8(fact_spo_key(&lunaris_core::Scope::dev(), &eid(SUBJ, SUBJ_TY).0, PRED))
            .unwrap();
    assert!(
        rec.rows.lock().contains_key(want.as_bytes()),
        "graph-ON ingest must write the spo index at {want}; keys were {:?}",
        rec.kv_keys_matching("factspo")
    );
}

/// A different object with an OVERLAPPING window across episodes is a
/// cross-episode contradiction — flagged to the verify queue for ASYNC
/// arbitration (blueprint subsystem 5), NOT closed synchronously.
#[tokio::test]
async fn graph_on_ingest_flags_cross_episode_contradiction() {
    let rec = Arc::new(RecordingStorage::default());
    let ex = Arc::new(ScriptedExtractor::default());
    let clock = HlcClock::new(0);
    let h = make(rec.clone(), ex.clone(), clock.clone());

    ex.set("muscovado", "2023-05-01");
    ingest_one(&h, &clock, "s1.md", "The user prefers muscovado.", (2023, 5, 1)).await;
    ex.set("turbinado", "2023-06-01"); // different object, both windows open => overlap
    ingest_one(&h, &clock, "s2.md", "The user now prefers turbinado.", (2023, 6, 1)).await;

    let msgs: Vec<serde_json::Value> = rec
        .published
        .lock()
        .iter()
        .filter(|(t, _)| t == "__lunaris_verify__")
        .map(|(_, v)| v.clone())
        .collect();
    assert!(
        !msgs.is_empty(),
        "changed preference across episodes must publish a verify-queue item"
    );
    let body = serde_json::to_string(&msgs).unwrap();
    assert!(
        body.contains("cross_episode_contradiction") || body.contains("CrossEpisodeContradiction"),
        "verify item must be the cross-episode-contradiction reason; got {body}"
    );
    assert!(body.contains(PRED), "contradiction must name the contested predicate; got {body}");

    // Both facts remain stored (additive) — arbitration is the verifier's job.
    assert_eq!(
        rec.kv_keys_matching(":fact:").len(),
        2,
        "differing objects are two distinct facts; the verifier closes the loser later"
    );
}

/// INGEST-04: the spo-index reads must NOT introduce a second atomic_write.
#[tokio::test]
async fn reconciliation_preserves_single_atomic_write_per_ingest() {
    let rec = Arc::new(RecordingStorage::default());
    let ex = Arc::new(ScriptedExtractor::default());
    let clock = HlcClock::new(0);
    let h = make(rec.clone(), ex.clone(), clock.clone());

    ex.set("muscovado", "2023-05-01");
    ingest_one(&h, &clock, "s1.md", "The user prefers muscovado.", (2023, 5, 1)).await;
    assert_eq!(rec.batches.lock().len(), 1, "ingest #1: exactly one atomic_write");

    ex.set("turbinado", "2023-06-01");
    ingest_one(&h, &clock, "s2.md", "The user now prefers turbinado.", (2023, 6, 1)).await;
    assert_eq!(rec.batches.lock().len(), 2, "ingest #2: exactly one MORE atomic_write");
}

/// Fail-open: a fact with no resolvable `valid_from` (no extraction date, no
/// episode `t_ref`) is written additively and skipped by reconciliation —
/// never a false supersede.
#[tokio::test]
async fn undated_fact_skips_reconciliation_without_false_supersede() {
    let rec = Arc::new(RecordingStorage::default());
    let ex = Arc::new(ScriptedExtractor::default());
    let clock = HlcClock::new(0);
    let h = make(rec.clone(), ex.clone(), clock.clone());

    ex.set("muscovado", "2023-05-01");
    ingest_one(&h, &clock, "s1.md", "The user prefers muscovado.", (2023, 5, 1)).await;

    // Undated episode AND undated extraction.
    ex.set("turbinado", "");
    let ep =
        Episode::new(lunaris_core::Scope::dev(), "s2.md", "The user mentions turbinado.", &clock);
    assert!(ep.t_ref.is_none());
    h.ingest(ep).await.expect("undated graph-ON ingest must succeed");

    let msgs: Vec<_> =
        rec.published.lock().iter().filter(|(t, _)| t == "__lunaris_verify__").cloned().collect();
    assert!(msgs.is_empty(), "an unresolvable window must not be classified at all; got {msgs:?}");
    assert_eq!(rec.kv_keys_matching(":fact:").len(), 2, "the fact is still written additively");
}
