//! Plan 03 / Task 1 stub — real impl in Task 3.
//!
//! Task 3 fills this with `GRAPH.QUERY` + optional `TEMPORAL.SNAPSHOT_AT` for AS_OF.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{CypherQuery, GraphResult};

use crate::client::MoonClient;

pub(crate) async fn graph_traverse(
    _c: &MoonClient,
    _query: &CypherQuery,
    _as_of: Option<Hlc>,
) -> Result<GraphResult, StorageError> {
    Err(StorageError::NotSupported(
        "Plan 03 Task 1 stub — filled in Task 3",
    ))
}
