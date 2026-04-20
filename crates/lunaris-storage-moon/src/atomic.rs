//! `atomic_write` — `TXN.BEGIN` + per-op fan-out + `TXN.COMMIT` (or `TXN.ABORT` on error).
//!
//! Each `WriteOp` translates to one Moon command:
//!
//! | variant            | command(s)                                    |
//! |--------------------|-----------------------------------------------|
//! | `KvPut`            | `HSET <key> v <value>` (single-field hash)    |
//! | `KvDelete`         | `DEL <key>`                                   |
//! | `VectorUpsert`     | `FT.UPSERT <index> <id> <le-f32-bytes> <metadata-json>` |
//! | `GraphNode`        | `GRAPH.QUERY <graph> "MERGE (n:{label} {id: $id}) SET n += $props"` |
//! | `GraphEdge`        | `GRAPH.QUERY <graph> "MATCH (a {id:$src}),(b {id:$dst}) MERGE (a)-[r:{rel}]->(b) SET r += $props"` |
//!
//! ## Threat note (T-01-03-01) — Cypher injection
//!
//! `WriteOp::GraphNode { label, .. }` and `WriteOp::GraphEdge { rel, .. }` interpolate
//! `label` / `rel` directly into the Cypher string. v0 trusts callers to validate these
//! against `^[A-Za-z_][A-Za-z0-9_]*$` BEFORE calling `atomic_write`. Phase 4 (`OPS-04`
//! audit) moves the guard into the `StoragePort` trait. We accept this in v0 because
//! validating at the trait would force every backend (Moon + Postgres + future) to
//! re-implement the same regex.
//!
//! ## Atomicity model
//!
//! `TXN.BEGIN` is single-connection scoped; we run all per-op commands on the same
//! `ConnectionManager` instance to ensure Moon associates them with the same transaction
//! handle. Any per-op error short-circuits to `TXN.ABORT` then surfaces the original
//! error.

use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{Lsn, WriteOp};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use crate::client::{MoonClient, redis_err};

pub(crate) async fn atomic_write(c: &MoonClient, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
    let mut conn = c.conn();

    // 1) TXN.BEGIN — opens a Moon transaction on this connection.
    redis::cmd("TXN.BEGIN").query_async::<String>(&mut conn).await.map_err(redis_err)?;

    // 2) Per-op fan-out. On any per-op error, ABORT and bubble the original error.
    if let Err(e) = run_ops(&mut conn, ops).await {
        // Best-effort abort; ignore its error (the transaction's already broken).
        let _ = redis::cmd("TXN.ABORT").query_async::<String>(&mut conn).await;
        return Err(e);
    }

    // 3) TXN.COMMIT — Moon may return either a packed u64 LSN (wall_ms<<32 | counter)
    //    or a simple "OK"; accept both. When "OK", fall back to a wall-clock LSN so
    //    callers always see a non-zero, monotonically-derivable Lsn.
    match redis::cmd("TXN.COMMIT").query_async::<redis::Value>(&mut conn).await {
        Ok(redis::Value::Int(n)) => {
            let n = n as u64;
            Ok(Lsn { wall_ms: n >> 32, counter: (n & 0xFFFF_FFFF) as u32 })
        }
        Ok(_) => {
            // "OK" / SimpleString / BulkString — fall back to client wall clock.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            Ok(Lsn { wall_ms: now_ms, counter: 0 })
        }
        Err(e) => Err(redis_err(e)),
    }
}

async fn run_ops(conn: &mut ConnectionManager, ops: &[WriteOp]) -> Result<(), StorageError> {
    for op in ops {
        match op {
            WriteOp::KvPut { key, value } => {
                // Single-field hash so HGET <key> v is the canonical read in `read_as_of`.
                let _: () =
                    conn.hset(key.as_slice(), "v", value.as_slice()).await.map_err(redis_err)?;
            }
            WriteOp::KvDelete { key } => {
                let _: () = conn.del(key.as_slice()).await.map_err(redis_err)?;
            }
            WriteOp::VectorUpsert { index, id, embedding, metadata } => {
                if embedding.is_empty() {
                    return Err(StorageError::Backend("vector embedding is empty".into()));
                }
                // Encode embedding as little-endian f32 bytes per Moon FT convention.
                let mut buf = Vec::with_capacity(embedding.len() * 4);
                for f in embedding {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
                let meta_json = serde_json::to_string(metadata)?;
                redis::cmd("FT.UPSERT")
                    .arg(index.as_str())
                    .arg(id.as_slice())
                    .arg(&buf)
                    .arg(meta_json)
                    .query_async::<redis::Value>(conn)
                    .await
                    .map_err(redis_err)?;
            }
            WriteOp::GraphNode { graph, id, label, props } => {
                // T-01-03-01: caller-validated `label`. See module rustdoc above.
                let props_json = serde_json::to_string(props)?;
                let cypher = format!("MERGE (n:{label} {{id: $id}}) SET n += $props");
                let params =
                    format!(r#"{{"id":"{}","props":{}}}"#, String::from_utf8_lossy(id), props_json);
                redis::cmd("GRAPH.QUERY")
                    .arg(graph.as_str())
                    .arg(cypher)
                    .arg("--params")
                    .arg(params)
                    .query_async::<redis::Value>(conn)
                    .await
                    .map_err(redis_err)?;
            }
            WriteOp::GraphEdge { graph, src, dst, rel, props } => {
                // T-01-03-01: caller-validated `rel`. See module rustdoc above.
                let props_json = serde_json::to_string(props)?;
                let cypher = format!(
                    "MATCH (a {{id:$src}}),(b {{id:$dst}}) MERGE (a)-[r:{rel}]->(b) SET r += $props",
                );
                let params = format!(
                    r#"{{"src":"{}","dst":"{}","props":{}}}"#,
                    String::from_utf8_lossy(src),
                    String::from_utf8_lossy(dst),
                    props_json
                );
                redis::cmd("GRAPH.QUERY")
                    .arg(graph.as_str())
                    .arg(cypher)
                    .arg("--params")
                    .arg(params)
                    .query_async::<redis::Value>(conn)
                    .await
                    .map_err(redis_err)?;
            }
        }
    }
    Ok(())
}
