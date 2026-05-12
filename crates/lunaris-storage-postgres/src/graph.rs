//! `graph_traverse` — AGE `cypher()` SQL function.
//!
//! TODO(v0.4): switch from the single-column `AS (v ag_catalog.agtype)` shape
//! to the multi-column `AS (id agtype, name agtype, type agtype, path_length
//! agtype, edge_weight_product agtype)` shape so `Graph::anchored`'s positional
//! `id`/`name`/`type` reads plus the optional `path_length` /
//! `edge_weight_product` header lookups all see real values. The current
//! single-column path is a pre-existing latent gap (positional reads of
//! `row.get(0).as_str()` against the AGE JSON blob produce empty-id hits);
//! see `docs/v0.3-known-debt.md` § "Graph scoring".
//!
//! SQL pattern:
//!
//! ```sql
//! SELECT * FROM cypher('lunaris_graph', $$ MATCH (n) RETURN n LIMIT 10 $$)
//!   AS (v ag_catalog.agtype);
//! ```
//!
//! For v0 we always return a single column `v agtype`. The `_as_of` parameter is accepted
//! but ignored — AGE has no native AS_OF; Phase 2 may layer a snapshot semantics on top
//! by filtering on `valid_from` columns when graph nodes also live in primitive tables.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::types::{CypherQuery, GraphResult};
use sqlx::{AssertSqlSafe, Row};

use crate::pool::{PgClient, sqlx_err};

pub(crate) async fn graph_traverse(
    c: &PgClient,
    scope: &Scope,
    query: &CypherQuery,
    _as_of: Option<Hlc>,
) -> Result<GraphResult, StorageError> {
    // The Cypher payload may contain `$` (parameter syntax). The sqlx layer doesn't
    // interpret `$` inside the AGE `$$ ... $$` string-literal envelope, so no escaping
    // is required here for sqlx itself. Passing the cypher verbatim.
    let sql = format!(
        "SELECT * FROM cypher('{graph}', $$ {cypher} $$) AS (v ag_catalog.agtype)",
        graph = query.graph,
        cypher = query.cypher,
    );

    // RFC 0001 Wave 1B: SET LOCAL inside a transaction so the scope GUC is
    // scoped to this connection use only.
    let mut tx = c.pool.begin().await.map_err(sqlx_err)?;
    // `SET LOCAL` cannot be parameterized in Postgres; use set_config() with
    // is_local=true, which is transaction-scoped (equivalent to SET LOCAL).
    sqlx::query("SELECT set_config('lunaris.scope', $1, true)")
        .bind(scope.as_str())
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

    let rows = sqlx::query(AssertSqlSafe(sql)).fetch_all(&mut *tx).await.map_err(sqlx_err)?;
    tx.commit().await.map_err(sqlx_err)?;

    let mut out_rows: Vec<Vec<serde_json::Value>> = Vec::new();
    for r in rows {
        // AGE's agtype maps to a textual JSON-ish form; sqlx-postgres reads it as a String.
        let v_str: String = r.try_get::<String, _>("v").unwrap_or_default();
        let parsed: serde_json::Value = match serde_json::from_str(&v_str) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(v_str),
        };
        out_rows.push(vec![parsed]);
    }

    Ok(GraphResult { headers: vec!["v".into()], rows: out_rows })
}
