//! `memory-update-intelligence` — the async supersede COMPLETES against
//! real (full-key) storage.
//!
//! This is the discriminating "built ≠ wired" test for the final half of the
//! feature: detection + publish (proven in the lunaris crate) route a
//! contradiction to the verifier, which arbitrates and calls
//! `worker::apply_supersede` to CLOSE the loser's bi-temporal interval.
//!
//! The production ingest path writes fact rows at the CANONICAL key
//! `lunaris:{scope}:fact:{ulid}` (`lunaris_core::keyspace::fact_key`). This
//! test seeds the loser + winner at THOSE keys — exactly as ingest does — then
//! drives `apply_supersede` and asserts the loser's `payload["bt"].sys[1]` is
//! closed (non-null). Under the old bug `apply_supersede` minted the scope-less
//! key for BOTH the `read_as_of` (→ miss → tombstone branch) AND the write, so
//! the canonical row was never read nor closed and stayed open → recall-as-of-now
//! would still return the superseded fact. This test fails under that bug.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use parking_lot::Mutex;

use lunaris_core::keyspace::fact_key;
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Hlc, HlcClock, Scope, StorageCapabilities, StorageError, StoragePort,
};
use lunaris_verify::VerifierBackend;
use lunaris_verify::cross_episode_decision;
use lunaris_verify::worker::apply_supersede;
use ulid::Ulid;

#[derive(Clone)]
struct MockRow {
    value: Vec<u8>,
    bt: BiTemporal,
}

/// Full-key storage double — mirrors the real embedded / Moon / PG convention
/// that the scope is carried INSIDE the `lunaris:{scope}:…` key (read_as_of
/// matches the literal key, ignoring the scope arg — exactly like the embedded
/// backend).
#[derive(Default)]
struct FullKeyStorage {
    rows: Mutex<HashMap<Vec<u8>, MockRow>>,
}

impl FullKeyStorage {
    fn seed(&self, key: Vec<u8>, value: Vec<u8>, bt: BiTemporal) {
        self.rows.lock().insert(key, MockRow { value, bt });
    }
    fn value_at(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.rows.lock().get(key).map(|r| r.value.clone())
    }
}

#[async_trait]
impl StoragePort for FullKeyStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        let mut rows = self.rows.lock();
        for op in ops {
            if let WriteOp::KvPut { key, value } = op {
                let bt = serde_json::from_slice::<serde_json::Value>(value)
                    .ok()
                    .and_then(|v| serde_json::from_value::<BiTemporal>(v["bt"].clone()).ok())
                    .unwrap_or_else(|| BiTemporal::at(Hlc::ZERO, Hlc::ZERO));
                rows.insert(key.clone(), MockRow { value: value.clone(), bt });
            }
        }
        Ok(Lsn { wall_ms: 1, counter: 0 })
    }

    async fn vector_search(
        &self,
        _s: &Scope,
        _i: &str,
        _q: &[f32],
        _k: usize,
        _f: Option<&Filter>,
        _a: Option<Hlc>,
        _r: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _s: &Scope,
        _q: &CypherQuery,
        _a: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _s: &Scope,
        _p: &[u8],
        _a: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new())))
    }

    async fn read_as_of(
        &self,
        _s: &Scope,
        key: &[u8],
        _a: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|r| Row {
            key: key.to_vec(),
            value: Bytes::from(r.value),
            bt: r.bt,
        }))
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

    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
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

fn fact_payload(text: &str, bt: &BiTemporal) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "fact_text": text,
        "bt": bt,
    }))
    .unwrap()
}

/// `sys[1]` (sys_to) of a persisted payload's `bt` — `None` if still open.
fn sys_to_is_closed(value: &[u8]) -> bool {
    let v: serde_json::Value = serde_json::from_slice(value).expect("payload is JSON");
    !v["bt"]["sys"][1].is_null()
}

#[tokio::test]
async fn supersede_closes_loser_at_canonical_fact_key() {
    let clock = HlcClock::new(1);
    let scope = Scope::new("agent-supersede").unwrap();

    let loser = Ulid::new();
    let winner = Ulid::new();
    let open = BiTemporal::at(Hlc::ZERO, Hlc::ZERO);

    // Seed BOTH rows at the canonical production key `lunaris:{scope}:fact:{ulid}`
    // (exactly where structured ingest writes them). A concrete handle so we
    // can call the `seed` helper; `apply_supersede` then takes the `dyn` view.
    let storage = Arc::new(FullKeyStorage::default());
    let loser_key = fact_key(&scope, loser);
    let winner_key = fact_key(&scope, winner);
    storage.seed(loser_key.clone(), fact_payload("Alice employer Acme (loser)", &open), open);
    storage.seed(winner_key.clone(), fact_payload("Alice employer Globex (winner)", &open), open);

    // latest-assertion-wins: new=winner supersedes existing=loser.
    let decision = cross_episode_decision(loser, winner, VerifierBackend::Candle);
    assert!(decision.applies(), "a cross-episode decision must arbitrate");

    let storage_dyn: Arc<dyn StoragePort> = storage.clone();
    apply_supersede(&storage_dyn, &decision, &clock, "fact", &scope)
        .await
        .expect("apply_supersede must succeed");

    // The loser row AT THE CANONICAL KEY must be closed (sys_to set). With a
    // verifier that reads a non-canonical key, this row is never touched and
    // the assertion fails — recall-as-of-now would still surface the loser.
    let loser_after =
        storage.value_at(&loser_key).expect("loser row still present at canonical key");
    assert!(
        sys_to_is_closed(&loser_after),
        "supersede must close the loser's bi-temporal sys interval at the canonical fact key"
    );

    // The winner row stays present (and open).
    let winner_after = storage.value_at(&winner_key).expect("winner row present at canonical key");
    assert!(
        !sys_to_is_closed(&winner_after),
        "winner must remain open (sys_to null) after supersede"
    );
}
