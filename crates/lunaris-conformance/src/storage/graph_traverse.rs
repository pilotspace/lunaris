//! Plan 05-02 D-17 + D-12 — `graph_traverse` conformance (capability-gated).
//!
//! Seeds 2 entity nodes + 1 relation edge via `fixtures::seed_one_edge`
//! (B-3 fix — defined in Task 1, called unconditionally here), then
//! issues a portable Cypher subset query (`MATCH (n)-[r]->(m) RETURN n`
//! with no parameters per D-16 from Phase 3) and asserts the result has
//! at least one row.
//!
//! Caller MUST gate on `storage.capabilities().graph_native == true`;
//! the suite entry [`run_full_storage_suite`](crate::storage::run_full_storage_suite)
//! handles that gate.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::types::CypherQuery;
use lunaris_core::storage::StoragePort;

pub async fn cypher_subset(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    debug_assert!(
        storage.capabilities().graph_native,
        "caller must gate on graph_native"
    );

    crate::fixtures::seed_one_edge(storage).await?;

    let query = CypherQuery {
        graph: "lunaris_graph".to_string(),
        cypher: "MATCH (n)-[r]->(m) RETURN n LIMIT 5".to_string(),
        params: serde_json::Map::new(),
    };
    let result = storage.graph_traverse(&query, None).await?;

    anyhow::ensure!(
        !result.rows.is_empty(),
        "graph_traverse::cypher_subset: backend returned 0 rows for the seeded edge (headers={:?})",
        result.headers,
    );
    Ok(())
}
