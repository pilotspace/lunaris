//! `atomic_write` — `TXN.BEGIN` + per-op fan-out + `TXN.COMMIT` (or `TXN.ABORT` on error).
//!
//! Phase 1.5 retrofit (STORE-09): all per-op commands are now dispatched through the
//! typed `moon-client` SDK (`txn_begin / txn_commit / txn_abort`, `set / del`,
//! `vector().upsert`, `graph().query_with_params`).
//!
//! Each `WriteOp` translates to one Moon command:
//!
//! | variant            | typed call                                                |
//! |--------------------|-----------------------------------------------------------|
//! | `KvPut`            | `client.hset(key, "v", value)` (single-field hash to keep `read_as_of`'s `HGET v` shape) |
//! | `KvDelete`         | `client.del(key)`                                         |
//! | `VectorUpsert`     | `client.vector().upsert(index, id, le-f32-bytes, meta-json)` |
//! | `GraphNode`        | `client.graph().query_with_params(graph, "MERGE (n:{label} {id:$id}) SET n += $props", params_json)` |
//! | `GraphEdge`        | `client.graph().query_with_params(graph, "MATCH (a {id:$src}),(b {id:$dst}) MERGE (a)-[r:{rel}]->(b) SET r += $props", params_json)` |
//!
//! ## Storage-shape note: SET vs HSET
//!
//! Plan 1's hand-rolled impl stored KV entries via `HSET <key> v <value>` so it could
//! piggy-back the bi-temporal `bt` field on the same hash. Phase 1.5 keeps the same
//! single-hash convention via the typed `MoonClient::hset` calls because `read_as_of`
//! depends on the `HGET <key> v` / `HGET <key> bt` shape — see `kv.rs` rustdoc.
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
//! cloned `moon-client::MoonClient` handle (which holds a single
//! `redis::aio::MultiplexedConnection` instance) to ensure Moon associates them with
//! the same transaction handle. Any per-op error short-circuits to `TXN.ABORT` then
//! surfaces the original error.

use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{Lsn, WriteOp};

use crate::client::{MoonClient, moon_err};

pub(crate) async fn atomic_write(c: &MoonClient, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
    let mut typed = c.typed();

    // 1) TXN.BEGIN — opens a Moon transaction on this connection.
    typed.txn_begin().await.map_err(moon_err)?;

    // 2) Per-op fan-out. On any per-op error, ABORT and bubble the original error.
    if let Err(e) = run_ops(&mut typed, ops).await {
        // Best-effort abort; ignore its error (the transaction's already broken).
        let _ = typed.txn_abort().await;
        return Err(e);
    }

    // 3) TXN.COMMIT — Moon's typed `txn_commit()` returns `()` after server ack. We
    //    don't get a packed-LSN back through the typed API; fall back to a wall-clock
    //    LSN so callers always see a non-zero, monotonically-derivable Lsn.
    typed.txn_commit().await.map_err(moon_err)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    Ok(Lsn { wall_ms: now_ms, counter: 0 })
}

async fn run_ops(
    typed: &mut moon_client::MoonClient,
    ops: &[WriteOp],
) -> Result<(), StorageError> {
    for op in ops {
        match op {
            WriteOp::KvPut { key, value } => {
                // Single-field hash so HGET <key> v is the canonical read in `read_as_of`.
                let _: i64 = typed
                    .hset(key.as_slice(), "v", value.as_slice())
                    .await
                    .map_err(moon_err)?;
            }
            WriteOp::KvDelete { key } => {
                let _: i64 = typed.del(key.as_slice()).await.map_err(moon_err)?;
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
                typed
                    .vector()
                    .upsert(index.as_str(), id.as_slice(), &buf, &meta_json)
                    .await
                    .map_err(moon_err)?;
            }
            WriteOp::GraphNode { graph, id, label, props } => {
                // T-01-03-01: caller-validated `label`. See module rustdoc above.
                let props_json = serde_json::to_string(props)?;
                let cypher = format!("MERGE (n:{label} {{id: $id}}) SET n += $props");
                let params = format!(
                    r#"{{"id":"{}","props":{}}}"#,
                    String::from_utf8_lossy(id),
                    props_json
                );
                let _: redis::Value = typed
                    .graph()
                    .query_with_params(graph.as_str(), &cypher, &params)
                    .await
                    .map_err(moon_err)?;
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
                let _: redis::Value = typed
                    .graph()
                    .query_with_params(graph.as_str(), &cypher, &params)
                    .await
                    .map_err(moon_err)?;
            }
        }
    }
    Ok(())
}
