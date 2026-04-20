//! Plan 03 / Task 1 stub — real impl in Task 2.
//!
//! Task 2 fills this with `FT.SEARCH` + optional `TEMPORAL.SNAPSHOT_AT` for AS_OF queries.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Filter, VectorHit};

use crate::client::MoonClient;

pub(crate) async fn vector_search(
    _c: &MoonClient,
    _index: &str,
    _query: &[f32],
    _k: usize,
    _filter: Option<&Filter>,
    _as_of: Option<Hlc>,
    _rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 2",
    ))
}
