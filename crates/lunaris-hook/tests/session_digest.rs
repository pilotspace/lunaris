//! ADD task `session-start-digest` — contextd digest core (`build_digest`).
//!
//! `build_digest` turns the scope's most-recent, source-filtered episodes into
//! curated `ContextMemory` entries (via the shared `snippet` curation) that the
//! `SessionDigest` handler renders + injects at session start. The handler wraps
//! `build_digest` errors into `ContextResponse::empty()` — a digest failure must
//! never block session start — so we also prove the error path propagates here.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    BiTemporal, CypherDialect, Episode, Hlc, QueueMsg, Scope, StorageCapabilities, StorageError,
    StoragePort, keyspace,
};
use lunaris_hook::context::build_digest;
use ulid::Ulid;

struct ScanStubStorage {
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    fail_scan: bool,
}

#[async_trait]
impl StoragePort for ScanStubStorage {
    async fn atomic_write(
        &self,
        _scope: &Scope,
        _ops: &[lunaris_core::WriteOp],
    ) -> Result<lunaris_core::Lsn, StorageError> {
        Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
    }

    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&lunaris_core::Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<lunaris_core::VectorHit>, StorageError> {
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _query: &lunaris_core::CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<lunaris_core::GraphResult, StorageError> {
        Ok(lunaris_core::GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        if self.fail_scan {
            return Err(StorageError::Backend("scan boom".into()));
        }
        let prefix = prefix.to_vec();
        let hits: Vec<Result<(Bytes, Bytes), StorageError>> = self
            .rows
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| Ok((Bytes::from(k.clone()), Bytes::from(v.clone()))))
            .collect();
        Ok(Box::pin(futures::stream::iter(hits)))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<lunaris_core::Row<Bytes>>, StorageError> {
        Ok(None)
    }

    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(futures::stream::empty()))
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
            cypher_dialect: CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

fn decision_row(scope: &Scope, id: Ulid, decision: &str) -> (Vec<u8>, Vec<u8>) {
    // record_decision stores a JSON envelope {"decision": "...","rationale": ...}
    // that the shared snippet curation renders as `decision: …`.
    let content = serde_json::json!({ "decision": decision }).to_string();
    let ep = Episode {
        id,
        scope: scope.clone(),
        source: format!("decision:{}", scope.as_str()),
        content,
        t_ref: None,
        bt: BiTemporal { valid: (Hlc::ZERO, None), sys: (Hlc::ZERO, None) },
        metadata: serde_json::Map::new(),
    };
    (keyspace::episode_key(scope, id), serde_json::to_vec(&ep).unwrap())
}

#[tokio::test]
async fn build_digest_renders_curated_decisions_newest_first() {
    let scope = Scope::new("digest_ctx").unwrap();
    let storage = ScanStubStorage {
        rows: vec![
            decision_row(&scope, Ulid::from_parts(1000, 1), "adopt launchd for Moon"),
            decision_row(&scope, Ulid::from_parts(2000, 2), "stop maintaining MEMORY.md"),
        ],
        fail_scan: false,
    };

    let prefixes = vec!["decision:".to_owned()];
    let memories = build_digest(&storage, &scope, &prefixes, 8).await.expect("digest ok");

    assert_eq!(memories.len(), 2);
    // newest first
    assert!(memories[0].snippet.starts_with("decision:"), "snippet: {}", memories[0].snippet);
    assert!(memories[0].snippet.contains("stop maintaining MEMORY.md"));
    assert!(memories[1].snippet.contains("adopt launchd for Moon"));
    assert!(memories.iter().all(|m| m.source.starts_with("decision:")));
    assert!(memories.iter().all(|m| m.score == 1.0));
}

#[tokio::test]
async fn build_digest_propagates_scan_error() {
    let scope = Scope::new("digest_ctx").unwrap();
    let storage = ScanStubStorage { rows: Vec::new(), fail_scan: true };
    let prefixes = vec!["decision:".to_owned()];
    let out = build_digest(&storage, &scope, &prefixes, 8).await;
    assert!(out.is_err(), "scan error must propagate so the handler fails-to-empty");
}
