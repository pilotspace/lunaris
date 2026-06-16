//! `memory-update-intelligence` — production-path integration suite.
//!
//! These tests drive the REAL `structured_ingest::ingest_structured_inner`
//! write path against a recording `StoragePort` double (full-key storage,
//! `queue_native = true`, captured publishes). They are the "built ≠ wired"
//! proof: the dedup + cross-episode-contradiction logic must be invoked by the
//! production ingest path, not merely unit-correct in `reconcile`/`keyspace`.
//!
//! §4 scenarios covered here:
//!   - test_exact_reassertion_dedups
//!   - test_cross_episode_contradiction_publishes
//!   - test_temporal_succession_no_publish
//!   - test_cross_scope_no_reconcile
//!   - test_ingest_04_single_atomic_write

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use futures::stream::{self, BoxStream};
use parking_lot::Mutex;

use lunaris::EpisodeBuilder;
use lunaris::structured_ingest::{FactInput, StructuredIngest, ingest_structured_inner};
use lunaris_core::keyspace::fact_prefix;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, NoopEmbedder, Scope, StorageCapabilities, StorageError,
    StoragePort,
};
use lunaris_extract::types::{EntityId, FactId};
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Recording StoragePort double — full-key storage (mirrors the real embedded /
// Moon / PG convention: scope is carried inside the `lunaris:{scope}:…` key,
// NOT a separate column), counts atomic_write calls, captures publishes, and
// reports `queue_native = true` so `publish_needs_review` actually publishes.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockRow {
    value: Vec<u8>,
    bt: BiTemporal,
}

#[derive(Default)]
struct RecordingStorage {
    rows: Mutex<HashMap<Vec<u8>, MockRow>>,
    atomic_writes: AtomicU64,
    published: Mutex<Vec<(String, Bytes)>>,
}

impl RecordingStorage {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn atomic_write_count(&self) -> u64 {
        self.atomic_writes.load(Ordering::SeqCst)
    }

    /// Count live KV rows under a key prefix (e.g. the fact-row prefix
    /// `lunaris:{scope}:fact:` — which does NOT match `factspo:`).
    fn rows_under(&self, prefix: &[u8]) -> usize {
        self.rows.lock().keys().filter(|k| k.starts_with(prefix)).count()
    }

    fn verify_publishes(&self) -> Vec<Bytes> {
        self.published
            .lock()
            .iter()
            .filter(|(t, _)| t == "__lunaris_verify__")
            .map(|(_, p)| p.clone())
            .collect()
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        let mut rows = self.rows.lock();
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    let bt = serde_json::from_slice::<serde_json::Value>(value)
                        .ok()
                        .and_then(|v| serde_json::from_value::<BiTemporal>(v["bt"].clone()).ok())
                        .unwrap_or_else(|| BiTemporal::at(Hlc::ZERO, Hlc::ZERO));
                    rows.insert(key.clone(), MockRow { value: value.clone(), bt });
                }
                WriteOp::KvDelete { key } => {
                    rows.remove(key);
                }
                _ => {}
            }
        }
        self.atomic_writes.fetch_add(1, Ordering::SeqCst);
        Ok(Lsn { wall_ms: 1, counter: 0 })
    }

    async fn vector_search(
        &self,
        _scope: &Scope,
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
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new())))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|r| Row {
            key: key.to_vec(),
            value: Bytes::from(r.value),
            bt: r.bt,
        }))
    }

    async fn publish(
        &self,
        _scope: &Scope,
        topic: &str,
        _partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        let mut msgs = self.published.lock();
        msgs.push((topic.to_string(), payload));
        Ok(msgs.len() as u64)
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            // The whole point: enable the verify-queue publish path.
            queue_native: true,
            max_vector_dim: 768,
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
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn embedder() -> Arc<dyn Embedder> {
    Arc::new(NoopEmbedder::new(768)) as Arc<dyn Embedder>
}

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).single().unwrap()
}

/// A one-fact structured payload: `(Alice/Person, predicate, object/Org)`.
fn one_fact(
    source: &str,
    predicate: &str,
    object: &str,
    vf: DateTime<Utc>,
    vt: Option<DateTime<Utc>>,
) -> StructuredIngest {
    StructuredIngest::new(EpisodeBuilder::new(source, "memory-update integration content"))
        .with_facts(vec![FactInput {
            fact_text: format!("Alice {predicate} {object}"),
            subject_name: "Alice".into(),
            subject_type: "Person".into(),
            predicate: predicate.into(),
            object_name: object.into(),
            object_type: "Org".into(),
            confidence: 0.9,
            valid_from: vf,
            valid_to: vt,
        }])
}

/// Deterministic fact id for `(Alice/Person, predicate, object/Org)`.
fn expected_fact_id(predicate: &str, object: &str) -> Ulid {
    let sid = EntityId::from_name_and_type("Alice", "Person");
    let oid = EntityId::from_name_and_type(object, "Org");
    Ulid::from_bytes(FactId::from_triple(sid, predicate, oid).0)
}

async fn ingest(
    storage: &Arc<RecordingStorage>,
    clock: &Arc<HlcClock>,
    scope: &Scope,
    payload: StructuredIngest,
) {
    ingest_structured_inner(storage.as_ref(), embedder().as_ref(), clock, payload, scope.clone())
        .await
        .expect("ingest_structured_inner must succeed against the recording mock");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_exact_reassertion_dedups() {
    let storage = RecordingStorage::new();
    let clock = HlcClock::new(1);
    let scope = Scope::new("agent-dedup").unwrap();

    // Same triple, two separate ingest calls (different episodes / windows).
    ingest(&storage, &clock, &scope, one_fact("turn-1", "employer", "Acme", day(2020, 1, 1), None))
        .await;
    ingest(&storage, &clock, &scope, one_fact("turn-2", "employer", "Acme", day(2021, 6, 1), None))
        .await;

    // Deterministic FactId → the second write overwrites the same fact row:
    // exactly ONE live fact row, NOT two. (With the old `Ulid::new()` this
    // would be 2 — the discriminating assertion.)
    let n = storage.rows_under(&fact_prefix(&scope));
    assert_eq!(n, 1, "re-asserting an identical triple must dedup to ONE fact row, got {n}");

    // And no cross-episode contradiction was raised (same object → NOOP).
    assert!(storage.verify_publishes().is_empty(), "exact dup must not publish a contradiction");
}

#[tokio::test]
async fn test_cross_episode_contradiction_publishes() {
    let storage = RecordingStorage::new();
    let clock = HlcClock::new(1);
    let scope = Scope::new("agent-contradict").unwrap();

    // Acme [2020, open) then Globex [2023, open) → overlapping, different object.
    ingest(&storage, &clock, &scope, one_fact("turn-1", "employer", "Acme", day(2020, 1, 1), None))
        .await;
    ingest(
        &storage,
        &clock,
        &scope,
        one_fact("turn-2", "employer", "Globex", day(2023, 1, 1), None),
    )
    .await;

    let pubs = storage.verify_publishes();
    assert_eq!(
        pubs.len(),
        1,
        "an overlapping different-object assertion must publish exactly one contradiction"
    );

    let env: serde_json::Value = serde_json::from_slice(&pubs[0]).expect("verify envelope is JSON");
    assert_eq!(env["kind"], "fact", "envelope kind must be fact");
    let reason = &env["item"]["reason"]["CrossEpisodeContradiction"];
    assert!(
        !reason.is_null(),
        "reason must be CrossEpisodeContradiction; got {}",
        env["item"]["reason"]
    );
    assert_eq!(reason["predicate"], "employer");
    assert_eq!(
        reason["existing_fact_id"].as_str().unwrap(),
        expected_fact_id("employer", "Acme").to_string(),
        "existing (loser) fact id must be the prior Acme assertion"
    );
    assert_eq!(
        reason["new_fact_id"].as_str().unwrap(),
        expected_fact_id("employer", "Globex").to_string(),
        "new (winner) fact id must be the incoming Globex assertion"
    );

    // Both facts remain on disk — the supersede is async (the verifier closes
    // the loser later); ingest stays additive + atomic.
    assert_eq!(
        storage.rows_under(&fact_prefix(&scope)),
        2,
        "both fact rows retained pre-supersede"
    );
}

#[tokio::test]
async fn test_temporal_succession_no_publish() {
    let storage = RecordingStorage::new();
    let clock = HlcClock::new(1);
    let scope = Scope::new("agent-succession").unwrap();

    // Acme [2020, 2022) then Globex [2023, open) → DISJOINT → legitimate
    // temporal succession → additive, NEVER a contradiction.
    ingest(
        &storage,
        &clock,
        &scope,
        one_fact("turn-1", "employer", "Acme", day(2020, 1, 1), Some(day(2022, 1, 1))),
    )
    .await;
    ingest(
        &storage,
        &clock,
        &scope,
        one_fact("turn-2", "employer", "Globex", day(2023, 1, 1), None),
    )
    .await;

    assert!(
        storage.verify_publishes().is_empty(),
        "non-overlapping succession must NOT publish a contradiction (no_false_supersede)"
    );
    assert_eq!(storage.rows_under(&fact_prefix(&scope)), 2, "both succession facts retained");
}

#[tokio::test]
async fn test_cross_scope_no_reconcile() {
    let storage = RecordingStorage::new();
    let clock = HlcClock::new(1);
    let scope_a = Scope::new("agent-A").unwrap();
    let scope_b = Scope::new("agent-B").unwrap();

    // Same (subject, predicate), overlapping windows, different object — but in
    // DIFFERENT scopes. Detection is scope-local, so scope B must not see A's
    // row → ZERO contradictions.
    ingest(
        &storage,
        &clock,
        &scope_a,
        one_fact("turn-1", "employer", "Acme", day(2020, 1, 1), None),
    )
    .await;
    ingest(
        &storage,
        &clock,
        &scope_b,
        one_fact("turn-2", "employer", "Globex", day(2023, 1, 1), None),
    )
    .await;

    assert!(
        storage.verify_publishes().is_empty(),
        "cross-scope assertions must NEVER reconcile (cross_scope_reconcile)"
    );
    // A's row is untouched and isolated.
    assert_eq!(storage.rows_under(&fact_prefix(&scope_a)), 1, "scope A keeps its own fact row");
    assert_eq!(storage.rows_under(&fact_prefix(&scope_b)), 1, "scope B keeps its own fact row");
}

#[tokio::test]
async fn test_reasserted_narrowed_window_keeps_spo_index_fresh() {
    // BUG-1 regression (adversarial review): re-asserting an identical triple
    // with a NARROWED window is a dedup NOOP that overwrites the fact row in
    // place — the spo-index entry's window MUST track that, else a later fact
    // is classified against a STALE (still-open) window → false Supersede.
    let storage = RecordingStorage::new();
    let clock = HlcClock::new(1);
    let scope = Scope::new("agent-reassert").unwrap();

    // 1. Assert Acme open from 2020.
    ingest(&storage, &clock, &scope, one_fact("turn-1", "employer", "Acme", day(2020, 1, 1), None))
        .await;
    // 2. Re-assert the SAME triple but CLOSE the window at 2021 → dedup NOOP,
    //    fact row overwritten to [2020, 2021).
    ingest(
        &storage,
        &clock,
        &scope,
        one_fact("turn-2", "employer", "Acme", day(2020, 1, 1), Some(day(2021, 1, 1))),
    )
    .await;
    // 3. Now assert Globex from 2022 — DISJOINT from Acme's real [2020, 2021)
    //    window → must be additive, NEVER a (false) supersede.
    ingest(
        &storage,
        &clock,
        &scope,
        one_fact("turn-3", "employer", "Globex", day(2022, 1, 1), None),
    )
    .await;

    assert!(
        storage.verify_publishes().is_empty(),
        "a fact disjoint from the RE-ASSERTED (narrowed) window must not falsely supersede \
         (stale spo-index window would wrongly overlap)"
    );
    // Still exactly one Acme row (dedup) + one Globex row.
    assert_eq!(storage.rows_under(&fact_prefix(&scope)), 2, "one deduped Acme + one Globex");
}

#[tokio::test]
async fn test_ingest_04_single_atomic_write() {
    let storage = RecordingStorage::new();
    let clock = HlcClock::new(1);
    let scope = Scope::new("agent-ingest04").unwrap();

    // Seed prior state: (employer, Acme) [2020, open).
    ingest(&storage, &clock, &scope, one_fact("seed", "employer", "Acme", day(2020, 1, 1), None))
        .await;
    let before = storage.atomic_write_count();

    // ONE ingest call carrying a MIXED batch: a dup, a contradiction, and a
    // brand-new fact. INGEST-04: this must issue EXACTLY ONE atomic_write; the
    // contradiction leaves via publish only (never a 2nd write).
    let mixed = StructuredIngest::new(EpisodeBuilder::new("turn-mixed", "mixed batch content"))
        .with_facts(vec![
            // dup of the seed → NOOP
            FactInput {
                fact_text: "Alice employer Acme".into(),
                subject_name: "Alice".into(),
                subject_type: "Person".into(),
                predicate: "employer".into(),
                object_name: "Acme".into(),
                object_type: "Org".into(),
                confidence: 0.9,
                valid_from: day(2021, 1, 1),
                valid_to: None,
            },
            // contradiction: different object, overlapping window → Supersede
            FactInput {
                fact_text: "Alice employer Globex".into(),
                subject_name: "Alice".into(),
                subject_type: "Person".into(),
                predicate: "employer".into(),
                object_name: "Globex".into(),
                object_type: "Org".into(),
                confidence: 0.9,
                valid_from: day(2023, 1, 1),
                valid_to: None,
            },
            // brand-new (subject, predicate) → Append
            FactInput {
                fact_text: "Alice founder NewCo".into(),
                subject_name: "Alice".into(),
                subject_type: "Person".into(),
                predicate: "founder".into(),
                object_name: "NewCo".into(),
                object_type: "Org".into(),
                confidence: 0.9,
                valid_from: day(2024, 1, 1),
                valid_to: None,
            },
        ]);
    ingest(&storage, &clock, &scope, mixed).await;

    assert_eq!(
        storage.atomic_write_count() - before,
        1,
        "a mixed ingest (dup + contradiction + new) must issue EXACTLY ONE atomic_write (INGEST-04)"
    );
    // The contradiction left ONLY via publish, not a second write.
    assert_eq!(
        storage.verify_publishes().len(),
        1,
        "exactly one cross-episode contradiction published from the mixed batch"
    );
}
