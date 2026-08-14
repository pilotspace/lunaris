//! Shared test scaffolding for the 0.6.2 server-lifecycle suites.
//!
//! [`StubStorage`] is a `StoragePort` whose liveness probe and write path are
//! independently controllable — the two knobs the P0-2 (HTTP resilience) and
//! P0-3 (`/readyz` write canary) suites need:
//!
//! - `health_delay` / `health_fails` — model a backend that answers PING
//!   slowly, or not at all.
//! - `write_delay` / `write_fails` — model the wedge that motivated `/readyz`:
//!   a backend that ACCEPTS connections and answers PING but STALLS writes.
//!
//! Every other method is a trivial no-op; these suites only exercise
//! `/healthz`, `/readyz` and the middleware stack.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris::Lunaris;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, HlcClock, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};

#[derive(Default, Clone)]
pub struct StubStorage {
    /// How long `health_check` (the PING analogue) parks before answering.
    pub health_delay: Duration,
    /// `health_check` answers `Err` instead of `Ok`.
    pub health_fails: bool,
    /// How long `atomic_write` (the canary analogue) parks before answering.
    /// `Duration::MAX`-ish values model a permanently stalled write path.
    pub write_delay: Duration,
    /// `atomic_write` answers `Err` instead of `Ok`.
    pub write_fails: bool,
    /// Number of `atomic_write` calls seen — how the `/readyz` suite proves
    /// the canary is rate-limited and that `/healthz` stays write-free.
    pub writes: Arc<std::sync::atomic::AtomicUsize>,
}

impl StubStorage {
    pub fn healthy() -> Self {
        Self::default()
    }

    pub fn with_health_delay(mut self, d: Duration) -> Self {
        self.health_delay = d;
        self
    }

    pub fn with_write_delay(mut self, d: Duration) -> Self {
        self.write_delay = d;
        self
    }

    pub fn with_health_failure(mut self) -> Self {
        self.health_fails = true;
        self
    }

    pub fn with_write_failure(mut self) -> Self {
        self.write_fails = true;
        self
    }

    /// Clone the write counter so a test can observe it after the stub has
    /// been moved into the router.
    pub fn write_counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.writes.clone()
    }
}

#[async_trait]
impl StoragePort for StubStorage {
    async fn health_check(&self) -> Result<(), StorageError> {
        if !self.health_delay.is_zero() {
            tokio::time::sleep(self.health_delay).await;
        }
        if self.health_fails {
            return Err(StorageError::Backend("stub: storage unreachable".into()));
        }
        Ok(())
    }

    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        _ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        self.writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if !self.write_delay.is_zero() {
            tokio::time::sleep(self.write_delay).await;
        }
        if self.write_fails {
            return Err(StorageError::Backend("stub: write rejected".into()));
        }
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
        _query: &CypherQuery,
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
        Ok(Box::pin(stream::empty()))
    }

    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
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

#[async_trait]
impl KeywordPort for StubStorage {
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

/// Write an empty bearer-token map — the probe routes need no auth, but
/// `build()` still reads the file.
pub fn write_test_tokens_file(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("lunaris-{tag}-tokens-{}.json", ulid::Ulid::new()));
    std::fs::write(&path, serde_json::json!({}).to_string()).expect("write tokens file");
    path
}

/// A `Config` with production defaults, ready for per-test overrides.
pub fn test_config(tag: &str) -> lunaris_server::Config {
    lunaris_server::Config {
        bind: "127.0.0.1:0".to_string(),
        storage: "test://stub".to_string(),
        tokens_file: write_test_tokens_file(tag),
        rate_per_second: 10_000,
        rate_burst: 10_000,
        cors_origins: "*".to_string(),
        shutdown_grace_secs: 30,
        metrics_disabled: false,
        http_timeout_secs: 30,
        http_concurrency: 256,
    }
}

/// Build the REAL production router over a [`StubStorage`].
pub fn build_app(cfg: lunaris_server::Config, storage: StubStorage) -> axum::Router {
    let storage = Arc::new(storage);
    let lunaris = Arc::new(Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ));
    lunaris_server::build(cfg, lunaris)
}
