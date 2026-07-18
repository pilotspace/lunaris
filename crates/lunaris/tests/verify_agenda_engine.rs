//! engram-soul-loop task 6 (staleness-pass) — engine-level RED/GREEN suite
//! (`.add/tasks/staleness-pass/TASK.md` §4 test_plan,
//! `crates/lunaris/tests/verify_agenda_engine.rs` row). Exercises
//! `ScopedLunaris::upsert_verify_agenda` (the RMW writer) against a
//! `memory://`-shaped in-process `StoragePort` double — same fixture style
//! as `activation_ledger_engine.rs`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Lunaris, VerifyAgendaEntry};
use lunaris_core::keyspace::verify_agenda_key;
use lunaris_core::{
    BiTemporal, CypherDialect, CypherQuery, Filter, GraphResult, Hlc, HlcClock, Lsn, QueueMsg, Row,
    Scope, StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};
use parking_lot::Mutex;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Fixture — mirrors LedgerTestStorage from activation_ledger_engine.rs.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AgendaTestStorage {
    rows: Mutex<HashMap<Vec<u8>, Row<Bytes>>>,
    write_batches: Mutex<Vec<Vec<WriteOp>>>,
}

impl AgendaTestStorage {
    fn write_count(&self) -> usize {
        self.write_batches.lock().len()
    }

    fn last_batch(&self) -> Vec<WriteOp> {
        self.write_batches.lock().last().cloned().unwrap_or_default()
    }
}

#[async_trait]
impl StoragePort for AgendaTestStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        {
            let mut rows = self.rows.lock();
            for op in ops {
                match op {
                    WriteOp::KvPut { key, value } => {
                        rows.insert(
                            key.clone(),
                            Row {
                                key: key.clone(),
                                value: value.clone().into(),
                                bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
                            },
                        );
                    }
                    // engram-soul-loop task 7: remove_verify_agenda issues a
                    // real KvDelete — the fixture must honor it.
                    WriteOp::KvDelete { key } => {
                        rows.remove(key);
                    }
                    // `WriteOp` is `#[non_exhaustive]` — vector/graph ops are
                    // irrelevant to this KV-only fixture.
                    _ => {}
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
        Ok(vec![])
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
        Err(StorageError::NotSupported("AgendaTestStorage::subscribe"))
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

fn make_handle(storage: Arc<AgendaTestStorage>) -> Lunaris {
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(lunaris_core::StubEmbedder::new(4));
    let clock = HlcClock::new(0);
    Lunaris::with_parts(storage as Arc<dyn StoragePort>, embedder, clock)
}

fn entry(
    episode_id: Ulid,
    anchor_head: &str,
    current_head: &str,
    files: &[&str],
) -> VerifyAgendaEntry {
    VerifyAgendaEntry {
        episode_id,
        anchor_head: anchor_head.to_owned(),
        current_head: current_head.to_owned(),
        files: files.iter().map(|s| s.to_string()).collect(),
        first_seen_ms: 1_000,
        last_seen_ms: 1_000,
        v: 1,
    }
}

// ---------------------------------------------------------------------------
// Test 1 — one atomic batch write for the whole entries slice (INGEST-04
// sibling contract: "ONE atomic_write of KvPuts", mirrors
// record_activation_refs).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_writes_one_atomic_batch() {
    let scope = Scope::new("test.agenda-batch").unwrap();
    let storage = Arc::new(AgendaTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let entries = vec![
        entry(id_a, "a".repeat(40).as_str(), "b".repeat(40).as_str(), &["src/lib.rs"]),
        entry(id_b, "a".repeat(40).as_str(), "b".repeat(40).as_str(), &["src/main.rs"]),
    ];

    scoped.upsert_verify_agenda(&entries).await.expect("upsert_verify_agenda must succeed");

    assert_eq!(storage.write_count(), 1, "a 2-entry batch is exactly ONE atomic_write");
    assert_eq!(storage.last_batch().len(), 2, "the batch carries exactly 2 KvPut ops");

    let key_a = verify_agenda_key(&scope, id_a);
    let row = storage.rows.lock().get(&key_a).cloned().expect("agenda row for id_a must exist");
    let stored: VerifyAgendaEntry = serde_json::from_slice(&row.value).unwrap();
    assert_eq!(stored.episode_id, id_a);
    assert_eq!(stored.anchor_head, "a".repeat(40));
    assert_eq!(stored.current_head, "b".repeat(40));
    assert_eq!(stored.files, vec!["src/lib.rs".to_string()]);
    assert_eq!(stored.v, 1);
}

// ---------------------------------------------------------------------------
// Test 2 — RMW: a second upsert for the SAME episode_id preserves
// first_seen_ms and updates last_seen_ms (+ any other changed field).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_upsert_preserves_first_seen_updates_last_seen() {
    let scope = Scope::new("test.agenda-rmw").unwrap();
    let storage = Arc::new(AgendaTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id = Ulid::new();
    let anchor = "a".repeat(40);
    let head1 = "b".repeat(40);
    let head2 = "c".repeat(40);

    let mut first = entry(id, &anchor, &head1, &["src/lib.rs"]);
    first.first_seen_ms = 1_000;
    first.last_seen_ms = 1_000;
    scoped.upsert_verify_agenda(&[first]).await.expect("first upsert must succeed");
    assert_eq!(storage.write_count(), 1);

    let mut second = entry(id, &anchor, &head2, &["src/lib.rs"]);
    second.first_seen_ms = 9_999; // deliberately wrong — must be overwritten by the RMW read
    second.last_seen_ms = 9_999;
    scoped.upsert_verify_agenda(&[second]).await.expect("second upsert must succeed");
    assert_eq!(storage.write_count(), 2, "the second upsert is a second atomic_write");

    let key = verify_agenda_key(&scope, id);
    let row = storage.rows.lock().get(&key).cloned().expect("agenda row must exist");
    let stored: VerifyAgendaEntry = serde_json::from_slice(&row.value).unwrap();
    assert_eq!(
        stored.first_seen_ms, 1_000,
        "first_seen_ms must be preserved across the RMW upsert"
    );
    assert_eq!(stored.last_seen_ms, 9_999, "last_seen_ms must reflect the new upsert value");
    assert_eq!(stored.current_head, head2, "current_head must reflect the new upsert value");
}

// ---------------------------------------------------------------------------
// Test 3 — an empty entries slice is a no-op (no atomic_write at all),
// mirroring record_activation_refs' empty-signals short-circuit.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_entries_is_noop() {
    let scope = Scope::new("test.agenda-empty").unwrap();
    let storage = Arc::new(AgendaTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    scoped.upsert_verify_agenda(&[]).await.expect("empty upsert must succeed");
    assert_eq!(storage.write_count(), 0, "an empty entries slice must not call atomic_write");
}

// ---------------------------------------------------------------------------
// engram-soul-loop task 7 (verify-agenda-tools) — list_verify_agenda +
// remove_verify_agenda (`.add/tasks/verify-agenda-tools/TASK.md` §4
// test_plan "handle.rs (memory:// engine)" row).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 4 — list_verify_agenda round-trips upserted entries, sorted by
// last_seen_ms DESC (freshest staleness first).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_verify_agenda_round_trips_sorted_by_last_seen_desc() {
    let scope = Scope::new("test.agenda-list").unwrap();
    let storage = Arc::new(AgendaTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id_older = Ulid::new();
    let id_newer = Ulid::new();
    let mut older = entry(id_older, &"a".repeat(40), &"b".repeat(40), &["src/a.rs"]);
    older.first_seen_ms = 1_000;
    older.last_seen_ms = 1_000;
    let mut newer = entry(id_newer, &"a".repeat(40), &"b".repeat(40), &["src/b.rs"]);
    newer.first_seen_ms = 2_000;
    newer.last_seen_ms = 5_000;

    scoped.upsert_verify_agenda(&[older, newer]).await.expect("seed upsert must succeed");

    let listed = scoped.list_verify_agenda().await.expect("list_verify_agenda must succeed");
    assert_eq!(listed.len(), 2, "both seeded entries must round-trip");
    assert_eq!(listed[0].episode_id, id_newer, "freshest last_seen_ms (5_000) must sort first");
    assert_eq!(listed[1].episode_id, id_older, "older last_seen_ms (1_000) must sort second");
    assert!(
        listed[0].last_seen_ms >= listed[1].last_seen_ms,
        "list_verify_agenda must be sorted last_seen_ms DESC"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — remove_verify_agenda returns true-then-false (idempotent) and the
// key is gone from storage after the first call.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_verify_agenda_is_idempotent_true_then_false() {
    let scope = Scope::new("test.agenda-remove").unwrap();
    let storage = Arc::new(AgendaTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id = Ulid::new();
    let seeded = entry(id, &"a".repeat(40), &"b".repeat(40), &["src/lib.rs"]);
    scoped.upsert_verify_agenda(&[seeded]).await.expect("seed upsert must succeed");

    let key = verify_agenda_key(&scope, id);
    assert!(storage.rows.lock().contains_key(&key), "agenda row must exist after seeding");

    let first = scoped.remove_verify_agenda(id).await.expect("first remove must succeed");
    assert!(first, "first remove_verify_agenda call must report the row existed");
    assert!(
        !storage.rows.lock().contains_key(&key),
        "the verify_agenda key must be gone from storage after removal"
    );

    let second = scoped.remove_verify_agenda(id).await.expect("second remove must succeed");
    assert!(!second, "a repeat remove_verify_agenda call on an absent row must return false");
}
