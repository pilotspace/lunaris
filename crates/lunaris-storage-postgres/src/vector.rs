//! `vector_search` — pgvector cosine distance + bi-temporal predicate.
//!
//! Task 1 ships the function signature; Task 2 fills in the SQL.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Filter, VectorHit};

use crate::pool::PgClient;

pub(crate) async fn vector_search(
    _c: &PgClient,
    _index: &str,
    _query: &[f32],
    _k: usize,
    _filter: Option<&Filter>,
    _as_of: Option<Hlc>,
    _rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    Err(StorageError::NotSupported("filled in Task 2"))
}
