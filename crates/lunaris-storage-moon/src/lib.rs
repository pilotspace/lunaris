//! lunaris-storage-moon — `MoonStorage` skeleton for Phase 1 / Plan 02.
//!
//! This file contains only the type definition, the `connect()` constructor that
//! verifies the URL parses, and a `StoragePort` impl whose IO methods all return
//! `StorageError::NotSupported("Phase 1 skeleton — implementation lands in Plan 03")`.
//! Plan 03 replaces the IO bodies with real Moon RESP commands; the type and
//! `capabilities()` shape locked here are stable.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row, StorageCapabilities, StorageError,
    StoragePort, VectorHit, WriteOp,
};

/// Skeleton MoonStorage handle — Plan 03 fills in the RESP client.
#[derive(Debug)]
pub struct MoonStorage {
    url: String,
}

impl MoonStorage {
    /// Validate the URL and construct a handle. Plan 03 replaces this body with a real
    /// `redis::aio::ConnectionManager::new(...)` call.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("moon parse: {e}")))?;
        if parsed.scheme() != "moon" {
            return Err(StorageError::UnsupportedScheme(parsed.scheme().into()));
        }
        Ok(Self { url: url.to_string() })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

#[async_trait]
impl StoragePort for MoonStorage {
    async fn atomic_write(&self, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
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
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
    }
    async fn graph_traverse(
        &self,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
    }
    async fn scan_range(
        &self,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
    }
    async fn read_as_of(&self, _k: &[u8], _as_of: Hlc) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
    }
    async fn publish(&self, _t: &str, _p: u16, _payload: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
    }
    async fn subscribe(
        &self,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("Phase 1 skeleton — Plan 03 fills MoonStorage IO"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: true,
            rerank_native: true,
            queue_native: true,
            max_vector_dim: 768,
        }
    }
}
