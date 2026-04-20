//! `graph_traverse` — AGE `cypher()` SQL function.
//!
//! Task 1 ships the function signature; Task 3 fills in the SQL.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{CypherQuery, GraphResult};

use crate::pool::PgClient;

pub(crate) async fn graph_traverse(
    _c: &PgClient,
    _query: &CypherQuery,
    _as_of: Option<Hlc>,
) -> Result<GraphResult, StorageError> {
    Err(StorageError::NotSupported("filled in Task 3"))
}
