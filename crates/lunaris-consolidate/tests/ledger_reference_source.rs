//! ADD task activation-ledger — RED/GREEN suite for scenario 8: "ACT-R
//! worker reads ledger references." `LedgerReferenceSource` feeds
//! `ActRConsolidator` from the persistent activation ledger instead of
//! `ConsolidateEvent` write-frequency signals, so promote/archive decisions
//! reflect USE.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_consolidate::{
    ActRConsolidator, LedgerReferenceSource, synthesize_fact_id_from_episode,
};
use lunaris_core::activation::{ActivationRecord, Grain, RefSignal};
use lunaris_core::keyspace::activation_key;
use lunaris_core::{
    CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row, Scope, StorageCapabilities,
    StorageError, StoragePort, VectorHit, WriteOp,
};
use parking_lot::Mutex;
use ulid::Ulid;

#[derive(Default)]
struct ScanOnlyStorage {
    rows: Mutex<HashMap<Vec<u8>, Bytes>>,
}

impl ScanOnlyStorage {
    fn seed(&self, key: Vec<u8>, record: &ActivationRecord) {
        self.rows.lock().insert(key, Bytes::from(serde_json::to_vec(record).unwrap()));
    }
}

#[async_trait]
impl StoragePort for ScanOnlyStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        unimplemented!("not exercised by this test")
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
        unimplemented!("not exercised by this test")
    }
    async fn graph_traverse(
        &self,
        _s: &Scope,
        _q: &CypherQuery,
        _t: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        unimplemented!("not exercised by this test")
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
            .map(|(k, v)| Ok((Bytes::from(k.clone()), v.clone())))
            .collect();
        Ok(stream::iter(matches).boxed())
    }
    async fn read_as_of(
        &self,
        _s: &Scope,
        _key: &[u8],
        _t: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        unimplemented!("not exercised by this test")
    }
    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        unimplemented!("not exercised by this test")
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        unimplemented!("not exercised by this test")
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

/// Given ledger records in scope S — one heavily-referenced (many recent
/// strong refs) and one unreferenced-and-old (single weak ref, ancient wall
/// time) — the ActRConsolidator's ledger-driven tick promotes the former and
/// archives the latter, using the ledger's weighted/wall summary as the
/// reference signal (not write-frequency `ConsolidateEvent`s).
#[tokio::test]
async fn act_r_tick_promotes_from_ledger_refs() {
    let scope = Scope::new("test.ledger-act-r").unwrap();
    let storage = Arc::new(ScanOnlyStorage::default());

    let now: u64 = 1_000_000;

    // Heavily-referenced: 20 strong refs, all very recent (elapsed ~= 1s
    // floor). weighted = 20 * 3.0 = 60; activation = ln(60 * 1^-0.5) = ln(60)
    // ~= 4.09, comfortably above the default promote_threshold=1.0.
    let heavy_id = Ulid::new();
    let mut heavy = ActivationRecord::default();
    for _ in 0..20 {
        heavy.apply(
            &RefSignal {
                id: heavy_id,
                grain: Grain::Turn,
                strength: lunaris_core::activation::Strength::Strong,
            },
            now - 1,
        );
    }
    storage.seed(activation_key(&scope, heavy_id), &heavy);

    // Unreferenced and old: one weak ref, one hour stale. weighted = 1.0;
    // activation = ln(1.0 * 3600^-0.5) ~= -4.09, comfortably below the
    // default archive_threshold=-0.5.
    let old_id = Ulid::new();
    let mut old = ActivationRecord::default();
    old.apply(
        &RefSignal {
            id: old_id,
            grain: Grain::Turn,
            strength: lunaris_core::activation::Strength::Weak,
        },
        now - 3600,
    );
    storage.seed(activation_key(&scope, old_id), &old);

    let source = LedgerReferenceSource::new(storage.clone() as Arc<dyn StoragePort>);
    let consolidator = ActRConsolidator::default();
    let report = consolidator
        .tick_from_ledger(&source, &scope, now)
        .await
        .expect("tick_from_ledger must succeed");

    assert!(
        report.promotions.iter().any(|p| p.episode_id == heavy_id),
        "heavily-referenced record must promote: {report:#?}"
    );
    let old_fact_id = synthesize_fact_id_from_episode(old_id);
    assert!(
        report.archives.iter().any(|a| a.fact_id == old_fact_id),
        "unreferenced old record must archive: {report:#?}"
    );
}
