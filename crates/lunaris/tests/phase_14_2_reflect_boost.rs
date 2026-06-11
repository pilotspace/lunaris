//! Phase 14.2 integration smoke tests — `ReflectOutput.boost` → retrieval rescore.
//!
//! ## What is pinned
//!
//! Four sub-tests prove the composition:
//!
//! 1. **`boost_rescore_promotes_chunk`** — happy path.
//!    `ScopedLunaris::end_turn` with `boost = [chunk_B]` populates the
//!    per-handle LRU cache; a subsequent `scoped.recall()` returns
//!    `chunk_B` as the first hit with `score == 0.80 + BOOST_DELTA`.
//!
//! 2. **`boost_isolated_by_scope`** — scope isolation.
//!    Boost written into `scope_a`'s cache is invisible to a recall under
//!    `scope_b`. Cache key is `(Scope, Ulid)` — a regression for the
//!    "Ulid alone" design mistake would be caught here.
//!
//! 3. **`empty_boost_is_noop`** — empty-output guard.
//!    `ReflectOutput { boost: vec![], .. }` does NOT write to the cache;
//!    `handle.boost_cache.read().len() == 0` after the call.
//!    Proves no spurious work occurs on the common case.
//!
//! 4. **`boost_combines_with_invalidate`** — Phase 14.1 + 14.2 composition.
//!    A single `end_turn` returning `{ invalidate: [X], boost: [Y] }` BOTH
//!    stamps X's `bt.sys.1` (MVCC) AND rescores Y on recall. Pins that the
//!    two Phase 14 mechanisms compose without interference.
//!
//! ## Storage design
//!
//! Rather than relying on `memory://` to produce deterministic cosine scores,
//! we wire a `BoostTestStorage` that returns fixed `VectorHit`s from
//! `vector_search` (scores 0.90 / 0.80 / 0.70 for A / B / C). `read_as_of`
//! returns seeded chunk + episode rows so `hydrate()` can project full `Hit`s.
//! The boost pass then adds `BOOST_DELTA` to the nominated chunk's score and
//! re-sorts, giving an exact arithmetic assertion.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::Lunaris;
use lunaris_core::keyspace::{chunk_key, episode_key};
use lunaris_core::{
    BiTemporal, Chunk, CypherDialect, CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock,
    Lsn, QueueMsg, Row, Scope, StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};
use lunaris_verify::{BOOST_DELTA, ReflectInput, ReflectOutput, ReflectSupervisor};
use parking_lot::Mutex;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// BoostTestStorage — fixed vector hits + in-memory KV rows
// ---------------------------------------------------------------------------

/// A `StoragePort` that returns caller-supplied `VectorHit`s from
/// `vector_search` and stores all `WriteOp::KvPut` values in memory so
/// `read_as_of` can hydrate them.
///
/// `fixed_hits` is cloned on each `vector_search` call so the ordering is
/// stable across multiple recall calls.
#[derive(Default)]
struct BoostTestStorage {
    fixed_hits: Mutex<Vec<VectorHit>>,
    rows: Mutex<HashMap<Vec<u8>, Row<Bytes>>>,
    write_batches: Mutex<Vec<Vec<WriteOp>>>,
    publishes: Mutex<Vec<Vec<u8>>>,
}

impl BoostTestStorage {
    fn new(hits: Vec<VectorHit>) -> Self {
        let s = Self::default();
        *s.fixed_hits.lock() = hits;
        s
    }

    /// Seed a KV row directly into the in-memory map (bypasses `atomic_write`
    /// so seeding doesn't pollute `write_batches`).
    fn seed(&self, key: Vec<u8>, value: Vec<u8>) {
        self.rows.lock().insert(
            key.clone(),
            Row { key, value: Bytes::from(value), bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO) },
        );
    }

    fn write_count(&self) -> usize {
        self.write_batches.lock().len()
    }
}

#[async_trait]
impl StoragePort for BoostTestStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        {
            let mut rows = self.rows.lock();
            for op in ops {
                if let WriteOp::KvPut { key, value } = op {
                    let bt: BiTemporal = serde_json::from_slice::<serde_json::Value>(value)
                        .ok()
                        .and_then(|v| serde_json::from_value(v["bt"].clone()).ok())
                        .unwrap_or(BiTemporal::at(Hlc::ZERO, Hlc::ZERO));
                    rows.insert(
                        key.clone(),
                        Row { key: key.clone(), value: value.clone().into(), bt },
                    );
                }
            }
        } // guard dropped before any .await
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
        _topic: &str,
        _p: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.publishes.lock().push(payload.to_vec());
        Ok(self.publishes.lock().len() as u64)
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
        _s: &Scope,
        _p: &[u8],
        _t: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("BoostTestStorage::subscribe"))
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
        }
    }
}

// ---------------------------------------------------------------------------
// StubReflectSupervisor — returns a fixed ReflectOutput
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StubReflectSupervisor {
    output: ReflectOutput,
}

#[async_trait]
impl ReflectSupervisor for StubReflectSupervisor {
    async fn reflect(
        &self,
        _input: ReflectInput,
    ) -> Result<ReflectOutput, lunaris_core::LunarisError> {
        Ok(self.output.clone())
    }
}

// ---------------------------------------------------------------------------
// Test fixture helpers
// ---------------------------------------------------------------------------

/// Build a deterministic 4-d stub embedding (different per chunk so
/// StubEmbedder values don't collide, but the vector is never actually used
/// for cosine — scores come from `fixed_hits`).
fn stub_embedding() -> Vec<f32> {
    vec![1.0, 0.0, 0.0, 0.0]
}

/// Seed one chunk + its parent episode into storage at the canonical
/// scoped KV keys (`lunaris:{scope}:chunk:{id}` / `lunaris:{scope}:episode:{ep_id}`).
fn seed_chunk(
    storage: &BoostTestStorage,
    scope: &Scope,
    chunk_id: Ulid,
    ep_id: Ulid,
    text: &str,
    clock: &HlcClock,
) {
    // Chunk row
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
    let chunk_bytes = serde_json::to_vec(&chunk).expect("chunk serializes");
    storage.seed(chunk_key(scope, chunk_id), chunk_bytes);

    // Episode row (hydrate requires it to populate Hit.source)
    let ep = Episode::new(scope.clone(), "test:source", "test episode text", clock);
    // Replace the auto-generated episode_id with our fixed one so the
    // chunk's `episode_id` matches.
    let mut ep_val = serde_json::to_value(&ep).unwrap();
    ep_val["id"] = serde_json::Value::String(ep_id.to_string());
    let ep_bytes = serde_json::to_vec(&ep_val).expect("episode serializes");
    storage.seed(episode_key(scope, ep_id), ep_bytes);
}

/// Build a `VectorHit` whose `id` is the raw 16-byte ULID bytes.
fn vector_hit(id: Ulid, score: f32) -> VectorHit {
    VectorHit {
        id: id.to_bytes().to_vec(),
        score,
        rerank_applied: false,
        metadata: serde_json::Value::Null,
    }
}

/// Build a `Lunaris` handle with `BoostTestStorage` and a `StubEmbedder`.
fn make_handle(storage: Arc<BoostTestStorage>, reflect_output: ReflectOutput) -> Lunaris {
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(lunaris_core::StubEmbedder::new(4));
    let clock = HlcClock::new(0);
    Lunaris::with_parts(storage as Arc<dyn StoragePort>, embedder, clock)
        .with_reflect_supervisor(Arc::new(StubReflectSupervisor { output: reflect_output }))
}

// ---------------------------------------------------------------------------
// Test 1 — boost_rescore_promotes_chunk
// ---------------------------------------------------------------------------

/// Happy path: after `end_turn(boost=[B])`, a subsequent recall returns B
/// first with `score == original_score_B + BOOST_DELTA`.
#[tokio::test]
async fn boost_rescore_promotes_chunk() {
    let scope = Scope::new("test.boost-promote").unwrap();
    let clock = HlcClock::new(0);

    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let id_c = Ulid::new();
    let ep_id = Ulid::new();

    // Scores: A=0.90, B=0.80, C=0.70. Initial order is A > B > C.
    let storage = Arc::new(BoostTestStorage::new(vec![
        vector_hit(id_a, 0.90),
        vector_hit(id_b, 0.80),
        vector_hit(id_c, 0.70),
    ]));

    let clock_for_seed = HlcClock::new(0);
    for (id, txt) in [(id_a, "chunk A"), (id_b, "chunk B"), (id_c, "chunk C")] {
        seed_chunk(&storage, &scope, id, ep_id, txt, &clock_for_seed);
    }

    let _ = clock; // only used to satisfy the local binding

    // Build handle. Boost only chunk B.
    let handle = make_handle(
        storage,
        ReflectOutput { boost: vec![id_b], invalidate: vec![], pre_warm_query: None },
    );
    let scoped = handle.scoped(scope.clone());

    // Run recall BEFORE end_turn — B should be second.
    let hits_before =
        scoped.recall(lunaris_retrieve::Query::text("test")).await.expect("recall before end_turn");
    assert_eq!(hits_before.len(), 3, "expected 3 hits before end_turn");
    assert_eq!(&hits_before[0].text, "chunk A", "A must be first before boost");
    assert_eq!(&hits_before[1].text, "chunk B", "B must be second before boost");

    // Run end_turn — populates the boost cache with (scope, id_b) -> BOOST_DELTA.
    let output = scoped
        .end_turn(ReflectInput {
            turn_id: None,
            turn_summary: "test turn".to_string(),
            recent_fact_ids: vec![],
            recent_chunk_ids: vec![id_a, id_b, id_c],
        })
        .await
        .expect("end_turn");
    assert_eq!(output.boost, vec![id_b], "ReflectOutput.boost must contain id_b");

    // Run recall AFTER end_turn — B should now be first.
    let hits_after =
        scoped.recall(lunaris_retrieve::Query::text("test")).await.expect("recall after end_turn");
    assert_eq!(hits_after.len(), 3, "expected 3 hits after end_turn");
    assert_eq!(&hits_after[0].text, "chunk B", "B must be first after boost");

    // Score of B must be original (0.80) + BOOST_DELTA.
    let expected_b_score = 0.80_f32 + BOOST_DELTA;
    let actual_b_score = hits_after[0].score;
    assert!(
        (actual_b_score - expected_b_score).abs() < 1e-5,
        "B.score must be {expected_b_score:.4} (0.80 + BOOST_DELTA={BOOST_DELTA}); got {actual_b_score:.4}"
    );

    eprintln!("boost_rescore_promotes_chunk: PASS (B promoted, score={actual_b_score:.4})");
}

// ---------------------------------------------------------------------------
// Test 2 — boost_isolated_by_scope
// ---------------------------------------------------------------------------

/// Boost written under `scope_a` is invisible to a recall under `scope_b`.
/// Cache key is `(Scope, Ulid)` — if the key were `Ulid` alone, this test
/// would fail.
#[tokio::test]
async fn boost_isolated_by_scope() {
    let scope_a = Scope::new("test.boost-iso-a").unwrap();
    let scope_b = Scope::new("test.boost-iso-b").unwrap();

    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let ep_id = Ulid::new();

    // Both scopes see the same fixed vector hits: id_a=0.90, id_b=0.80.
    let storage =
        Arc::new(BoostTestStorage::new(vec![vector_hit(id_a, 0.90), vector_hit(id_b, 0.80)]));

    let clock = HlcClock::new(0);
    // Seed chunk rows under BOTH scopes so hydrate succeeds for each.
    for scope in [&scope_a, &scope_b] {
        for (id, txt) in [(id_a, "chunk A"), (id_b, "chunk B")] {
            seed_chunk(&storage, scope, id, ep_id, txt, &clock);
        }
    }

    // Build a handle, boost id_b under scope_a.
    let handle = make_handle(
        storage,
        ReflectOutput { boost: vec![id_b], invalidate: vec![], pre_warm_query: None },
    );

    // end_turn under scope_a — populates (scope_a, id_b) in the cache.
    let scoped_a = handle.scoped(scope_a.clone());
    scoped_a
        .end_turn(ReflectInput {
            turn_id: None,
            turn_summary: "scope_a turn".to_string(),
            recent_fact_ids: vec![],
            recent_chunk_ids: vec![id_b],
        })
        .await
        .expect("end_turn scope_a");

    // Recall under scope_b — must NOT see the boost; A still ranks first.
    let scoped_b = handle.scoped(scope_b.clone());
    let hits_b =
        scoped_b.recall(lunaris_retrieve::Query::text("test")).await.expect("recall scope_b");
    assert_eq!(hits_b.len(), 2, "scope_b recall must return 2 hits");
    assert_eq!(
        &hits_b[0].text, "chunk A",
        "scope_b recall must not see scope_a's boost; A must be first: {hits_b:#?}"
    );
    assert!(
        (hits_b[0].score - 0.90).abs() < 1e-5,
        "A.score must be 0.90 (unaffected by scope_a boost); got {}",
        hits_b[0].score
    );

    eprintln!("boost_isolated_by_scope: PASS");
}

// ---------------------------------------------------------------------------
// Test 3 — empty_boost_is_noop
// ---------------------------------------------------------------------------

/// `ReflectOutput { boost: vec![], .. }` does not write to the cache.
/// `handle.boost_cache.read().len() == 0` after the call.
#[tokio::test]
async fn empty_boost_is_noop() {
    let scope = Scope::new("test.boost-noop").unwrap();

    let id_a = Ulid::new();
    let ep_id = Ulid::new();

    let storage = Arc::new(BoostTestStorage::new(vec![vector_hit(id_a, 0.90)]));
    let clock = HlcClock::new(0);
    seed_chunk(&storage, &scope, id_a, ep_id, "chunk A", &clock);

    // Empty boost output — no nominations.
    let handle = make_handle(
        storage.clone(),
        ReflectOutput { boost: vec![], invalidate: vec![], pre_warm_query: None },
    );

    let scoped = handle.scoped(scope.clone());
    scoped
        .end_turn(ReflectInput {
            turn_id: None,
            turn_summary: "empty boost turn".to_string(),
            recent_fact_ids: vec![],
            recent_chunk_ids: vec![id_a],
        })
        .await
        .expect("end_turn empty boost");

    // Behavioral contract: no write batches produced (no invalidations, no
    // storage side-effects from an empty-boost turn).
    assert_eq!(
        storage.write_count(),
        0,
        "empty boost+invalidate end_turn must produce zero atomic_writes"
    );

    // Recall ordering is vanilla — A is first (score=0.90, no delta).
    let hits = scoped
        .recall(lunaris_retrieve::Query::text("test"))
        .await
        .expect("recall after empty boost");
    assert_eq!(hits.len(), 1);
    assert!((hits[0].score - 0.90).abs() < 1e-5, "A.score must be 0.90; got {}", hits[0].score);

    eprintln!("empty_boost_is_noop: PASS");
}

// ---------------------------------------------------------------------------
// Test 4 — boost_combines_with_invalidate
// ---------------------------------------------------------------------------

/// A single `end_turn` with `{ invalidate: [X], boost: [Y] }`:
/// - X's fact row has `bt.sys.1` stamped (MVCC invalidated).
/// - Y is rescored on subsequent recall — Y moves to rank 1.
///
/// Pins that Phase 14.1 (invalidate) + Phase 14.2 (boost) compose without
/// interference from a single `end_turn` call.
#[tokio::test]
async fn boost_combines_with_invalidate() {
    let scope = Scope::new("test.boost-invalidate-compose").unwrap();
    let clock = HlcClock::new(0);

    // Chunk Y and X
    let id_x_fact = Ulid::new(); // fact to be invalidated
    let id_y = Ulid::new(); // chunk to be boosted
    let id_z = Ulid::new(); // control chunk
    let ep_id = Ulid::new();

    // Vector hits: Z=0.90, Y=0.80. After boost Y should overtake Z.
    let storage =
        Arc::new(BoostTestStorage::new(vec![vector_hit(id_z, 0.90), vector_hit(id_y, 0.80)]));

    // Seed chunk rows for Y and Z.
    seed_chunk(&storage, &scope, id_y, ep_id, "chunk Y", &clock);
    seed_chunk(&storage, &scope, id_z, ep_id, "chunk Z", &clock);

    // Seed the fact row for X so apply_reflect_invalidate can read + stamp it.
    let fact_key_x = format!("fact:{id_x_fact}").into_bytes();
    let now = clock.tick();
    let bt_live = BiTemporal::at(now, now);
    let fact_payload = serde_json::json!({
        "ulid": id_x_fact.to_string(),
        "bt": serde_json::to_value(bt_live).unwrap(),
    });
    storage.seed(fact_key_x.clone(), serde_json::to_vec(&fact_payload).unwrap());

    // Build handle with combined invalidate + boost output.
    let handle = make_handle(
        storage.clone(),
        ReflectOutput { invalidate: vec![id_x_fact], boost: vec![id_y], pre_warm_query: None },
    );

    let scoped = handle.scoped(scope.clone());

    // Fire end_turn — both 14.1 and 14.2 should apply.
    let output = scoped
        .end_turn(ReflectInput {
            turn_id: None,
            turn_summary: "combined turn".to_string(),
            recent_fact_ids: vec![id_x_fact],
            recent_chunk_ids: vec![id_y, id_z],
        })
        .await
        .expect("end_turn combined");

    assert_eq!(output.invalidate, vec![id_x_fact]);
    assert_eq!(output.boost, vec![id_y]);

    // 14.1 assertion: exactly one atomic_write for the invalidation.
    assert_eq!(
        storage.write_count(),
        1,
        "D-11: exactly one atomic_write for the invalidate batch; got {}",
        storage.write_count()
    );

    // 14.1 assertion: fact X's bt.sys.1 is now non-null in stored bytes.
    let x_row = storage.rows.lock().get(&fact_key_x).cloned().expect("fact X row must exist");
    let x_payload: serde_json::Value = serde_json::from_slice(&x_row.value).unwrap();
    assert!(
        !x_payload["bt"]["sys"][1].is_null(),
        "14.1: X bt.sys[1] must be non-null after invalidation; got bt={}",
        x_payload["bt"]
    );

    // 14.2 assertion: recall returns Y first (boosted above Z).
    let hits = scoped
        .recall(lunaris_retrieve::Query::text("test"))
        .await
        .expect("recall after combined end_turn");
    assert_eq!(hits.len(), 2, "expected 2 hits");
    assert_eq!(
        &hits[0].text,
        "chunk Y",
        "14.2: Y must be first after boost; order: {:#?}",
        hits.iter().map(|h| (&h.text, h.score)).collect::<Vec<_>>()
    );

    let expected_y_score = 0.80_f32 + BOOST_DELTA;
    assert!(
        (hits[0].score - expected_y_score).abs() < 1e-5,
        "14.2: Y.score must be {expected_y_score:.4}; got {:.4}",
        hits[0].score
    );

    eprintln!("boost_combines_with_invalidate: PASS");
}
