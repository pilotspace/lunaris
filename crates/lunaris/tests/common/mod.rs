//! Shared fixture for the activation-ledger integration binaries.
//!
//! Lives under `tests/common/` (a subdirectory, so cargo does NOT compile it as
//! its own test binary) because two binaries need the same `StoragePort`
//! double: `activation_ledger_engine.rs` (the parallel suite) and
//! `activation_boost_optout.rs` (which mutates a process-global env var and
//! therefore MUST be alone in its binary).
//!
//! `dead_code` is allowed at module scope only: each consumer uses a different
//! subset of the helpers, and a per-binary unused warning here says nothing
//! about the fixture being wrong.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::Lunaris;
use lunaris_core::keyspace::{chunk_key, episode_key};
use lunaris_core::{
    BiTemporal, Chunk, CypherDialect, CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock,
    Lsn, QueueMsg, Row, Scope, StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
};
use parking_lot::Mutex;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Fixture — mirrors BoostTestStorage from phase_14_2_reflect_boost.rs.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct LedgerTestStorage {
    pub fixed_hits: Mutex<Vec<VectorHit>>,
    pub rows: Mutex<HashMap<Vec<u8>, Row<Bytes>>>,
    pub write_batches: Mutex<Vec<Vec<WriteOp>>>,
    /// Every key passed to `read_as_of`, in call order. W1.8 uses this to
    /// bound the per-recall ledger read cost that now lands on EVERY surface.
    pub reads: Mutex<Vec<Vec<u8>>>,
    /// When set, `atomic_write` always fails (reject-2 fixture).
    pub fail_writes: std::sync::atomic::AtomicBool,
}

impl LedgerTestStorage {
    pub fn new(hits: Vec<VectorHit>) -> Self {
        let s = Self::default();
        *s.fixed_hits.lock() = hits;
        s
    }

    pub fn seed(&self, key: Vec<u8>, value: Vec<u8>) {
        self.rows.lock().insert(
            key.clone(),
            Row { key, value: Bytes::from(value), bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO) },
        );
    }

    /// Keys read through `read_as_of` that are activation-ledger rows.
    pub fn ledger_reads(&self, scope: &Scope) -> Vec<Vec<u8>> {
        let prefix = format!("lunaris:{}:activation:", scope.as_str()).into_bytes();
        self.reads.lock().iter().filter(|k| k.starts_with(&prefix)).cloned().collect()
    }

    pub fn write_count(&self) -> usize {
        self.write_batches.lock().len()
    }

    pub fn last_batch(&self) -> Vec<WriteOp> {
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
        self.reads.lock().push(key.to_vec());
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

pub fn stub_embedding() -> Vec<f32> {
    vec![1.0, 0.0, 0.0, 0.0]
}

pub fn seed_chunk(
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

pub fn vector_hit(id: Ulid, score: f32) -> VectorHit {
    VectorHit {
        id: id.to_bytes().to_vec(),
        score,
        rerank_applied: false,
        metadata: serde_json::Value::Null,
    }
}

pub fn make_handle(storage: Arc<LedgerTestStorage>) -> Lunaris {
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(lunaris_core::StubEmbedder::new(4));
    let clock = HlcClock::new(0);
    Lunaris::with_parts(storage as Arc<dyn StoragePort>, embedder, clock)
}
