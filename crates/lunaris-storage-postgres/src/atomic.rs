//! `atomic_write` — sqlx Transaction, per-op INSERT/UPSERT, COMMIT/ROLLBACK.
//!
//! Each `WriteOp` translates to one statement on the same `sqlx::Transaction`:
//!
//! | variant            | SQL                                                                       |
//! |--------------------|---------------------------------------------------------------------------|
//! | `KvPut`            | `INSERT INTO lunaris_kv ... ON CONFLICT (key) DO UPDATE SET ...`          |
//! | `KvDelete`         | `UPDATE lunaris_kv SET valid_to = $now, sys_to = $now WHERE key = $1 AND valid_to IS NULL` |
//! | `VectorUpsert`     | `INSERT INTO {chunks|entities|facts|communities} ... ON CONFLICT (id) DO UPDATE` |
//! | `GraphNode`        | `SELECT * FROM cypher('graph', $$ MERGE (n:label {id:'..'}) SET n += {..} $$) AS (v agtype)` |
//! | `GraphEdge`        | `SELECT * FROM cypher('graph', $$ MATCH ... MERGE (a)-[r:rel]->(b) SET r += {..} $$) AS (v agtype)` |
//!
//! ## Threat note (T-01-04-02 / T-01-04-03) — Cypher + index-name injection
//!
//! `index` (vector table name) is whitelist-matched against `chunks|entities|facts|communities`;
//! anything else returns `StorageError::Backend`. `label` / `rel` in Cypher are caller-validated
//! against `^[A-Za-z_][A-Za-z0-9_]*$` per the same v0 contract as `lunaris-storage-moon` —
//! Phase 4 OPS-04 moves the regex into the trait so every backend re-uses it.
//!
//! ## Atomicity
//!
//! All ops run on the same `sqlx::Transaction`; `tx.commit()` is called once at the end.
//! Any per-op error short-circuits with `?`, which drops the transaction (sqlx auto-rolls
//! back on drop). The returned `Lsn` is derived from the `HlcClock::tick()` issued at the
//! start of the batch — single commit point, total order across writes.
//!
//! ## Scope partitioning (RFC 0001 Wave 1B)
//!
//! `SET LOCAL lunaris.scope = $1` is issued inside every transaction immediately after
//! `BEGIN`. Postgres RLS policies on every primitive table use
//! `current_setting('lunaris.scope', true)` to enforce per-scope isolation. `SET LOCAL`
//! ensures the GUC reverts when the transaction ends — no cross-transaction contamination.

use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::HlcClock;
use lunaris_core::scope::Scope;
use lunaris_core::storage::types::{Lsn, WriteOp};

use sqlx::AssertSqlSafe;

use crate::pool::{PgClient, sqlx_err};
use crate::schema::{BtRow, hlc_to_ts};

pub(crate) async fn atomic_write(
    c: &PgClient,
    scope: &Scope,
    ops: &[WriteOp],
) -> Result<Lsn, StorageError> {
    let mut tx = c.pool.begin().await.map_err(sqlx_err)?;

    // RFC 0001 Wave 1B: set the scope GUC for this transaction so every RLS
    // policy on primitive tables filters to the correct scope. SET LOCAL reverts
    // automatically when the transaction ends — no cross-request contamination.
    sqlx::query("SET LOCAL lunaris.scope = $1")
        .bind(scope.as_str())
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

    // Single LSN HLC for the whole batch — all rows share valid_from / sys_from.
    let clock = HlcClock::new(0);
    let lsn_hlc = clock.tick();
    let now_ts = hlc_to_ts(lsn_hlc);
    let bt = BiTemporal { valid: (lsn_hlc, None), sys: (lsn_hlc, None) };
    let bt_json = serde_json::to_value(bt)?;
    let row = BtRow::from(&bt);

    for op in ops {
        match op {
            WriteOp::KvPut { key, value } => {
                sqlx::query(
                    "INSERT INTO lunaris_kv \
                       (key, value, bt, valid_from, valid_to, sys_from, sys_to, valid_from_hlc, sys_from_hlc) \
                     VALUES ($1, $2, $3, $4, NULL, $5, NULL, $6, $7) \
                     ON CONFLICT (key) DO UPDATE SET \
                       value = EXCLUDED.value, \
                       bt = EXCLUDED.bt, \
                       valid_from = EXCLUDED.valid_from, \
                       sys_from = EXCLUDED.sys_from, \
                       valid_to = NULL, \
                       sys_to = NULL, \
                       valid_from_hlc = EXCLUDED.valid_from_hlc, \
                       sys_from_hlc   = EXCLUDED.sys_from_hlc",
                )
                .bind(key.as_slice())
                .bind(value.as_slice())
                .bind(&bt_json)
                .bind(row.valid_from_ts)
                .bind(row.sys_from_ts)
                .bind(row.valid_from_hlc)
                .bind(row.sys_from_hlc)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_err)?;
            }
            WriteOp::KvDelete { key } => {
                // Soft delete: close the open interval on the latest row.
                sqlx::query(
                    "UPDATE lunaris_kv SET valid_to = $2, sys_to = $2 \
                     WHERE key = $1 AND valid_to IS NULL",
                )
                .bind(key.as_slice())
                .bind(now_ts)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_err)?;
            }
            WriteOp::VectorUpsert { index, id, embedding, metadata } => {
                if embedding.is_empty() {
                    return Err(StorageError::Backend("vector embedding is empty".into()));
                }
                // `index` selects the table: "chunks" | "entities" | "facts" | "communities".
                // T-01-04-02 mitigation: whitelist match.
                let (table, embedding_col): (&str, &str) = match index.as_str() {
                    "chunks" => ("chunks", "embedding"),
                    "entities" => ("entities", "embedding"),
                    "facts" => ("facts", "embedding"),
                    "communities" => ("communities", "summary_embedding"),
                    other => {
                        return Err(StorageError::Backend(format!(
                            "unknown vector index: {other} (valid: chunks|entities|facts|communities)"
                        )));
                    }
                };
                // pgvector accepts the literal string form: '[v0,v1,...]'
                let emb_lit = vec_to_literal(embedding);
                // RFC 0001 Wave 1B: scope column is included in INSERT.
                // The RLS SET LOCAL above also enforces the scope during the
                // INSERT; storing scope in the row allows future migrations to
                // rebuild RLS policies without reprocessing data.
                let sql = format!(
                    "INSERT INTO {table} \
                       (id, scope, payload, {embedding_col}, valid_from, valid_to, sys_from, sys_to, valid_from_hlc, sys_from_hlc) \
                     VALUES ($1, $2, $3, $4::vector, $5, NULL, $6, NULL, $7, $8) \
                     ON CONFLICT (id) DO UPDATE SET \
                       scope = EXCLUDED.scope, \
                       payload = EXCLUDED.payload, \
                       {embedding_col} = EXCLUDED.{embedding_col}, \
                       valid_from = EXCLUDED.valid_from, \
                       sys_from = EXCLUDED.sys_from, \
                       valid_from_hlc = EXCLUDED.valid_from_hlc, \
                       sys_from_hlc   = EXCLUDED.sys_from_hlc"
                );
                sqlx::query(AssertSqlSafe(sql))
                    .bind(id.as_slice())
                    .bind(scope.as_str())
                    .bind(metadata)
                    .bind(emb_lit)
                    .bind(row.valid_from_ts)
                    .bind(row.sys_from_ts)
                    .bind(row.valid_from_hlc)
                    .bind(row.sys_from_hlc)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_err)?;
            }
            WriteOp::GraphNode { graph, id, label, props } => {
                // T-01-04-03: caller-validated `label`. See module rustdoc above.
                let cypher = format!(
                    "MERGE (n:{label} {{id: '{id_str}'}}) SET n += {props_json}",
                    id_str = String::from_utf8_lossy(id),
                    props_json = serde_json::to_string(props)?,
                );
                let sql = format!(
                    "SELECT * FROM cypher('{graph}', $$ {cypher} $$) AS (v ag_catalog.agtype)"
                );
                sqlx::query(AssertSqlSafe(sql)).execute(&mut *tx).await.map_err(sqlx_err)?;
            }
            WriteOp::GraphEdge { graph, src, dst, rel, props } => {
                // T-01-04-03: caller-validated `rel`. See module rustdoc above.
                let cypher = format!(
                    "MATCH (a {{id:'{src_str}'}}),(b {{id:'{dst_str}'}}) MERGE (a)-[r:{rel}]->(b) SET r += {props_json}",
                    src_str = String::from_utf8_lossy(src),
                    dst_str = String::from_utf8_lossy(dst),
                    props_json = serde_json::to_string(props)?,
                );
                let sql = format!(
                    "SELECT * FROM cypher('{graph}', $$ {cypher} $$) AS (v ag_catalog.agtype)"
                );
                sqlx::query(AssertSqlSafe(sql)).execute(&mut *tx).await.map_err(sqlx_err)?;
            }
        }
    }

    tx.commit().await.map_err(sqlx_err)?;

    Ok(Lsn { wall_ms: lsn_hlc.wall_ms, counter: lsn_hlc.counter })
}

/// Format a `&[f32]` as the pgvector literal `'[v0,v1,...]'`.
fn vec_to_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    let mut first = true;
    for f in v {
        if !first {
            s.push(',');
        }
        first = false;
        // Use `{}` (Display) — pgvector parses standard f32 text.
        use std::fmt::Write as _;
        let _ = write!(s, "{f}");
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::vec_to_literal;

    #[test]
    fn vec_to_literal_format() {
        assert_eq!(vec_to_literal(&[]), "[]");
        assert_eq!(vec_to_literal(&[1.0]), "[1]");
        assert_eq!(vec_to_literal(&[1.0, 2.5, -0.5]), "[1,2.5,-0.5]");
    }
}
