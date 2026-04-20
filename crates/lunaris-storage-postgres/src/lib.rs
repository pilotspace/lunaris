//! lunaris-storage-postgres — `PostgresStorage` skeleton for Phase 1 / Plan 02.
//!
//! Plan 04 replaces the IO bodies with real `sqlx` queries against `pgvector` + `AGE` +
//! `pgmq` + bi-temporal columns. The type and `capabilities()` shape locked here are stable.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row, StorageCapabilities, StorageError,
    StoragePort, VectorHit, WriteOp,
};

/// Skeleton PostgresStorage handle — Plan 04 fills in the sqlx pool.
#[derive(Debug)]
pub struct PostgresStorage {
    url: String,
}

impl PostgresStorage {
    /// Validate the URL and construct a handle. Plan 04 replaces this body with a real
    /// `sqlx::postgres::PgPoolOptions::new().connect(...)` call + migrations.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("postgres parse: {e}")))?;
        if parsed.scheme() != "postgres" && parsed.scheme() != "postgresql" {
            return Err(StorageError::UnsupportedScheme(parsed.scheme().into()));
        }
        Ok(Self { url: url.to_string() })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

#[async_trait]
impl StoragePort for PostgresStorage {
    async fn atomic_write(&self, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    async fn vector_search(
        &self,
        _index: &str,
        _q: &[f32],
        _k: usize,
        _f: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    async fn graph_traverse(
        &self,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    async fn scan_range(
        &self,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    async fn read_as_of(&self, _k: &[u8], _as_of: Hlc) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    async fn publish(&self, _t: &str, _p: u16, _payload: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    async fn subscribe(
        &self,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 04 fills PostgresStorage IO"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false, // emulated via valid_from/valid_to/sys_from/sys_to columns
            graph_native: true,        // AGE
            rerank_native: false,      // no native cross-encoder
            queue_native: true,        // pgmq
            max_vector_dim: 1536,      // pgvector default upper bound
        }
    }
}
