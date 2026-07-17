//! ADD task activation-ledger — engine-level RED/GREEN suite (§4 test_plan,
//! `crates/lunaris/tests/activation_ledger_engine.rs` row). Exercises
//! `ScopedLunaris::record_activation_refs` (the write half) and
//! `lunaris_retrieve::LedgerBoostProvider` (the read half) against a
//! `memory://`-shaped in-process `StoragePort` double — same fixture style
//! as `phase_14_2_reflect_boost.rs`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::Lunaris;
use lunaris_core::activation::{ActivationRecord, Grain, RefSignal, Strength};
use lunaris_core::keyspace::{activation_key, chunk_key, episode_key};
use lunaris_core::{
    BiTemporal, Chunk, CypherDialect, CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock,
    Lsn, QueueMsg, Row, Scope, StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};
use lunaris_retrieve::{BoostProvider, LedgerBoostProvider, Query, Vector};
use parking_lot::Mutex;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Fixture — mirrors BoostTestStorage from phase_14_2_reflect_boost.rs.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LedgerTestStorage {
    fixed_hits: Mutex<Vec<VectorHit>>,
    rows: Mutex<HashMap<Vec<u8>, Row<Bytes>>>,
    write_batches: Mutex<Vec<Vec<WriteOp>>>,
    /// When set, `atomic_write` always fails (reject-2 fixture).
    fail_writes: std::sync::atomic::AtomicBool,
}

impl LedgerTestStorage {
    fn new(hits: Vec<VectorHit>) -> Self {
        let s = Self::default();
        *s.fixed_hits.lock() = hits;
        s
    }

    fn seed(&self, key: Vec<u8>, value: Vec<u8>) {
        self.rows.lock().insert(
            key.clone(),
            Row { key, value: Bytes::from(value), bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO) },
        );
    }

    fn write_count(&self) -> usize {
        self.write_batches.lock().len()
    }

    fn last_batch(&self) -> Vec<WriteOp> {
        self.write_batches.lock().last().cloned().unwrap_or_default()
    }
}

#[async_trait]
impl StoragePort for LedgerTestStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Backend(
                "ledger_test_storage: forced atomic_write failure".into(),
            ));
        }
        {
            let mut rows = self.rows.lock();
            for op in ops {
                if let WriteOp::KvPut { key, value } = op {
                    rows.insert(
                        key.clone(),
                        Row {
                            key: key.clone(),
                            value: value.clone().into(),
                            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
                        },
                    );
                }
            }
        }
        self.write_batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: self.write_batches.lock().len() as u32 })
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        key: &[u8],
        _t: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned())
    }

    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    #[allow(clippy::too_many_arguments)]
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
        Ok(self.fixed_hits.lock().clone())
    }

    async fn graph_traverse(
        &self,
        _s: &Scope,
        _q: &CypherQuery,
        _t: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        let rows = self.rows.lock();
        let matches: Vec<Result<(Bytes, Bytes), StorageError>> = rows
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| Ok((Bytes::from(k.clone()), v.value.clone())))
            .collect();
        Ok(stream::iter(matches).boxed())
    }

    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("LedgerTestStorage::subscribe"))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 4,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

fn stub_embedding() -> Vec<f32> {
    vec![1.0, 0.0, 0.0, 0.0]
}

fn seed_chunk(
    storage: &LedgerTestStorage,
    scope: &Scope,
    chunk_id: Ulid,
    ep_id: Ulid,
    text: &str,
    clock: &HlcClock,
) {
    let chunk = Chunk {
        id: chunk_id,
        scope: scope.clone(),
        episode_id: ep_id,
        text: text.to_string(),
        tokens: 5,
        offset: 0,
        heading_path: vec![],
        overlap_tail: String::new(),
        embedding: Some(stub_embedding()),
        bt: BiTemporal::now(clock),
        parent_id: None,
    };
    storage.seed(chunk_key(scope, chunk_id), serde_json::to_vec(&chunk).unwrap());

    let ep = Episode::new(scope.clone(), "test:source", "test episode text", clock);
    let mut ep_val = serde_json::to_value(&ep).unwrap();
    ep_val["id"] = serde_json::Value::String(ep_id.to_string());
    storage.seed(episode_key(scope, ep_id), serde_json::to_vec(&ep_val).unwrap());
}

fn vector_hit(id: Ulid, score: f32) -> VectorHit {
    VectorHit {
        id: id.to_bytes().to_vec(),
        score,
        rerank_applied: false,
        metadata: serde_json::Value::Null,
    }
}

fn make_handle(storage: Arc<LedgerTestStorage>) -> Lunaris {
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(lunaris_core::StubEmbedder::new(4));
    let clock = HlcClock::new(0);
    Lunaris::with_parts(storage as Arc<dyn StoragePort>, embedder, clock)
}

// ---------------------------------------------------------------------------
// Test 1 — scenarios 1+2: upsert math through the engine writer, batched flush.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn record_refs_upserts_and_batches_one_atomic_write() {
    let scope = Scope::new("test.ledger-upsert").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id_m = Ulid::new();

    // First signal: weak, turn-grain.
    scoped
        .record_activation_refs(&[RefSignal {
            id: id_m,
            grain: Grain::Turn,
            strength: Strength::Weak,
        }])
        .await
        .expect("first record_activation_refs must succeed");
    assert_eq!(storage.write_count(), 1, "first flush is one atomic_write");

    let key = activation_key(&scope, id_m);
    let row = storage.rows.lock().get(&key).cloned().expect("activation record must exist");
    let rec: ActivationRecord = serde_json::from_slice(&row.value).unwrap();
    assert_eq!(rec.n, 1);
    assert_eq!(rec.weighted, 1.0);
    assert_eq!(rec.last_grain, Grain::Turn);
    assert_eq!(rec.last_strength, Strength::Weak);
    assert_eq!(rec.first_ref_wall, rec.last_ref_wall, "both walls set identically on first signal");

    let first_wall = rec.first_ref_wall;

    // Second signal for the SAME id: strong, tool_call-grain — updates in place.
    scoped
        .record_activation_refs(&[RefSignal {
            id: id_m,
            grain: Grain::ToolCall,
            strength: Strength::Strong,
        }])
        .await
        .expect("second record_activation_refs must succeed");
    assert_eq!(storage.write_count(), 2, "second flush is a second atomic_write");

    let row2 = storage.rows.lock().get(&key).cloned().expect("activation record must still exist");
    let rec2: ActivationRecord = serde_json::from_slice(&row2.value).unwrap();
    assert_eq!(rec2.n, 2, "n increments");
    assert_eq!(rec2.weighted, 4.0, "weighted = weak(1.0) + strong(3.0)");
    assert_eq!(rec2.first_ref_wall, first_wall, "first_ref_wall unchanged");
    assert!(
        rec2.last_ref_wall >= first_wall,
        "last_ref_wall advances (or ties under a fast clock)"
    );
    assert_eq!(rec2.last_grain, Grain::ToolCall);
    assert_eq!(rec2.last_strength, Strength::Strong);

    // Batch of THREE distinct ids in ONE call flushes as exactly ONE atomic_write.
    let id_x = Ulid::new();
    let id_y = Ulid::new();
    let id_z = Ulid::new();
    scoped
        .record_activation_refs(&[
            RefSignal { id: id_x, grain: Grain::Turn, strength: Strength::Weak },
            RefSignal { id: id_y, grain: Grain::Turn, strength: Strength::Weak },
            RefSignal { id: id_z, grain: Grain::Turn, strength: Strength::Weak },
        ])
        .await
        .expect("batch record_activation_refs must succeed");
    assert_eq!(storage.write_count(), 3, "the 3-id batch is ONE additional atomic_write");
    assert_eq!(storage.last_batch().len(), 3, "the batch atomic_write carries exactly 3 KvPut ops");
}

// ---------------------------------------------------------------------------
// Test 2 — exit criterion: reinforced memory outranks an equal-similarity
// peer across a FRESH handle (no in-process cache carryover), real recall path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reinforced_memory_outranks_across_handles() {
    let scope = Scope::new("test.ledger-cross-handle").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let ep = Ulid::new();

    // B ranks first pre-boost (equal scores, B returned first) so any flip
    // to A is explained ONLY by the persisted activation ledger.
    let storage =
        Arc::new(LedgerTestStorage::new(vec![vector_hit(id_b, 0.80), vector_hit(id_a, 0.80)]));
    seed_chunk(&storage, &scope, id_a, ep, "chunk A", &clock);
    seed_chunk(&storage, &scope, id_b, ep, "chunk B", &clock);

    // Session 1: a fresh handle records two strong refs for A, then is dropped.
    {
        let handle1 = make_handle(storage.clone());
        let scoped1 = handle1.scoped(scope.clone());
        scoped1
            .record_activation_refs(&[
                RefSignal { id: id_a, grain: Grain::ToolCall, strength: Strength::Strong },
                RefSignal { id: id_a, grain: Grain::ToolCall, strength: Strength::Strong },
            ])
            .await
            .expect("session 1 record_activation_refs must succeed");
    } // handle1 dropped — its boost_cache is gone with it.

    // Session 2: a BRAND NEW handle over the SAME persisted storage. Its
    // boost_cache starts empty — any boost must come from the ledger, not an
    // in-process cache.
    let handle2 = make_handle(storage.clone());
    let scoped2 = handle2.scoped(scope.clone());

    let provider: Arc<dyn BoostProvider> =
        Arc::new(LedgerBoostProvider::new(storage.clone() as Arc<dyn StoragePort>));
    let hits = scoped2
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider)
        .execute(Query::text("q"))
        .await
        .expect("recall with ledger provider must succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].text, "chunk A",
        "A's persisted activation must outrank equal-similarity B: {hits:#?}"
    );

    // Without the provider wired, the pre-boost order (B first) is preserved.
    let hits_unwired =
        scoped2.dsl().with_root(Vector::new("chunks", 30)).execute(Query::text("q")).await.unwrap();
    assert_eq!(hits_unwired[0].text, "chunk B", "without the provider, pre-boost order must hold");
}

// ---------------------------------------------------------------------------
// Test 3 — reject: a corrupt activation record never fails recall.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_record_recall_still_ok() {
    let scope = Scope::new("test.ledger-corrupt").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let ep = Ulid::new();

    let storage = Arc::new(LedgerTestStorage::new(vec![vector_hit(id_a, 0.55)]));
    seed_chunk(&storage, &scope, id_a, ep, "chunk A", &clock);
    // Garbage payload at A's activation key.
    storage.seed(activation_key(&scope, id_a), b"{not json".to_vec());

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());
    let provider: Arc<dyn BoostProvider> =
        Arc::new(LedgerBoostProvider::new(storage.clone() as Arc<dyn StoragePort>));

    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider)
        .execute(Query::text("q"))
        .await
        .expect("recall must return Ok even with a corrupt ledger row");

    assert_eq!(hits.len(), 1);
    assert!(
        (hits[0].score - 0.55).abs() < 1e-6,
        "corrupt record must not change the hit's score; got {}",
        hits[0].score
    );
}

// ---------------------------------------------------------------------------
// Test 4 — reject: a ledger write failure surfaces as Err (the CALLER, e.g.
// lunaris-hook::trace_injection, is the one that must log-and-continue).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ledger_write_failure_does_not_error_turn_path() {
    let scope = Scope::new("test.ledger-write-fail").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    storage.fail_writes.store(true, std::sync::atomic::Ordering::SeqCst);

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let result = scoped
        .record_activation_refs(&[RefSignal {
            id: Ulid::new(),
            grain: Grain::Turn,
            strength: Strength::Weak,
        }])
        .await;
    assert!(
        result.is_err(),
        "record_activation_refs must surface the storage error to its caller — callers on the \
         turn path (trace_injection) are responsible for log-and-continue, matching the \
         apply_reflect_invalidate/apply_reflect_boost best-effort contract"
    );
}
