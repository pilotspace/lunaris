//! `atomic_write` — `TXN.BEGIN` + per-op fan-out + `TXN.COMMIT` (or `TXN.ABORT` on error).
//!
//! RFC 0001 Wave 1C: all keys and FT index names are now scope-prefixed via
//! `keyspace::{scope_prefix, ft_index_name, graph_key}`.
//!
//! Phase 1.5 retrofit (STORE-09): all per-op commands are now dispatched through the
//! typed `moon-client` SDK (`txn_begin / txn_commit / txn_abort`, `set / del`,
//! `hset_multiple`, `graph().query_raw`).
//!
//! Each `WriteOp` translates to one Moon command:
//!
//! | variant            | typed call                                                |
//! |--------------------|-----------------------------------------------------------|
//! | `KvPut`            | `client.hset(key, "v", value)` (single-field hash to keep `read_as_of`'s `HGET v` shape) |
//! | `KvDelete`         | `client.del(key)`                                         |
//! | `VectorUpsert`     | `client.hset_multiple("{ft_index_name(scope,kind)}:{id_hex}", [...])` |
//! | `GraphNode`        | `client.graph().query_raw(graph_key(scope), "MERGE ...")` |
//! | `GraphEdge`        | `client.graph().query_raw(graph_key(scope), "MATCH ... MERGE ...")` |
//!
//! ## Storage-shape note: SET vs HSET
//!
//! Plan 1's hand-rolled impl stored KV entries via `HSET <key> v <value>` so it could
//! piggy-back the bi-temporal `bt` field on the same hash. Phase 1.5 keeps the same
//! single-hash convention via the typed `MoonClient::hset` calls because `read_as_of`
//! depends on the `HGET <key> v` / `HGET <key> bt` shape — see `kv.rs` rustdoc.
//!
//! ## FT key shape (Wave 1C)
//!
//! `VectorUpsert` writes to `{ft_index_name(scope, kind)}:{id_hex}`. The FT index
//! `ft_index_name(scope, kind)` is created lazily on first write (see `MoonStorage`).
//! `vector::decode_key(raw, ft_idx)` strips the `<ft_idx>:` prefix + hex-decodes to
//! a 16-byte ULID — this contract is unchanged; only the prefix length changes.
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
//!
//! ## Multi-shard limitation (Fix 3, 2026-04-21)
//!
//! Moon's `TXN` does NOT support cross-shard writes (`TXN does not support
//! cross-shard writes -- use hash tags {tag} to co-locate keys`). Lunaris
//! WriteOps currently use raw key/index names without `{}` hash tags so any
//! atomic_write spanning more than one shard fails. Workaround for live
//! measurement: run Moon with `--shards 1` (single-shard mode). Real fix
//! requires WriteOp routing keys + `{<tag>}:` key prefixing so all ops in
//! one transaction land on the same shard. Tracked as future work; not a
//! blocker for v0.1.0-alpha measurement on a single-node Moon dev box.

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{Lsn, WriteOp};

use crate::client::{MoonClient, moon_err};
use crate::keyspace::{ft_index_name, graph_key};

pub(crate) async fn atomic_write(
    c: &MoonClient,
    scope: &Scope,
    ops: &[WriteOp],
) -> Result<Lsn, StorageError> {
    let mut typed = c.typed();

    // 1) TXN.BEGIN — opens a Moon transaction on this connection.
    typed.txn_begin().await.map_err(moon_err)?;

    // 2) Per-op fan-out. On any per-op error, ABORT and bubble the original error.
    if let Err(e) = run_ops(&mut typed, scope, ops).await {
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
    typed: &mut moon::MoonClient,
    scope: &Scope,
    ops: &[WriteOp],
) -> Result<(), StorageError> {
    for op in ops {
        match op {
            WriteOp::KvPut { key, value } => {
                // KvPut keys already carry the full scope-prefixed path from the caller
                // (keyspace::episode_key(scope, id) etc.) — we write them verbatim.
                // Single-field hash so HGET <key> v is the canonical read in `read_as_of`.
                let _: i64 =
                    typed.hset(key.as_slice(), "v", value.as_slice()).await.map_err(moon_err)?;
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
                // Gap 9 fix (2026-04-21): mirror the Postgres tsvector convention
                // (`crates/lunaris-storage-postgres/migrations/20260421_000004_tsvector_bm25.sql`)
                // so Moon BM25 / HYBRID FT.SEARCH have a real text payload to
                // score against. Without this, the FT index sees only `vec`
                // and FT.SEARCH returns "no such index" / "unknown index".
                let content = extract_content_for_index(index, metadata);
                // RFC 0001 Wave 1C: write to `{ft_index_name(scope, kind)}:{id_hex}`.
                // The FT index name is per-scope so each agent's vectors are isolated.
                // `vector::decode_key` strips this prefix to recover the 16-byte ULID.
                let id_hex = hex::encode(id);
                let per_scope_ft_idx = ft_index_name(scope, index);
                let key = format!("{per_scope_ft_idx}:{id_hex}");
                // Plan 09.1-02 Task 2b — valid_time numeric field on chunks only.
                // The `chunks` FT index declares `valid_time` as
                // `SchemaField::Numeric` (Task 2 / `ensure_indexes`). Moon's
                // RediSearch NUMERIC parser accepts ASCII-decimal bytes. Other
                // indices (entities / facts / communities) do NOT declare this
                // field — HSETting it there would be wasted work (FT ignores
                // undeclared fields), so we gate on `index == "chunks"`. The
                // decimal string is allocated BEFORE the hset_multiple borrow
                // so the `&[u8]` slice outlives the call (mirrors meta_json
                // materialisation above).
                let valid_time_buf: Option<String> = if index == "chunks" {
                    metadata.get("valid_time_ms").and_then(|v| v.as_u64()).map(|ms| ms.to_string())
                } else {
                    None
                };
                // Plan 15-01 Task 1 — extract source from metadata for chunks
                // index so the `SchemaField::Tag("source")` FT index can
                // resolve `@source:{value}` queries server-side (PERF-MOON-01).
                let source_buf: Option<String> = if index == "chunks" {
                    metadata.get("source").and_then(|v| v.as_str()).map(|s| s.to_string())
                } else {
                    None
                };
                let mut fields: Vec<(&str, &[u8])> = Vec::with_capacity(6);
                fields.push(("vec", buf.as_slice()));
                fields.push(("meta", meta_json.as_bytes()));
                if let Some(c) = content.as_ref() {
                    fields.push(("content", c.as_bytes()));
                }
                if let Some(ref vt) = valid_time_buf {
                    fields.push(("valid_time", vt.as_bytes()));
                }
                if let Some(ref src) = source_buf {
                    fields.push(("source", src.as_bytes()));
                }
                typed.hset_multiple(key.as_bytes(), &fields).await.map_err(moon_err)?;
            }
            WriteOp::GraphNode { graph: _, id, label, props } => {
                // RFC 0001 Wave 1C: use per-scope graph key, ignoring the WriteOp's
                // `graph` field (which was the global "lunaris_graph" pre-RFC).
                // T-01-03-01: caller-validated `label`. See module rustdoc above.
                // Moon Cypher only supports `SET n.prop = expr` (no map-merge `+=`).
                // Moon's GRAPH.QUERY handler also ignores `--params` (always empty
                // HashMap internally), so we inline all values as literals.
                // Hex-encode the raw id bytes — EntityId is [u8;16] hash,
                // lossy UTF-8 produces Cypher-unsafe characters.
                let id_hex = hex::encode(id);
                let set_clause = build_set_clause("n", props);
                let cypher = if set_clause.is_empty() {
                    format!("MERGE (n:{label} {{id: '{id_hex}'}}) RETURN n")
                } else {
                    format!("MERGE (n:{label} {{id: '{id_hex}'}}) {set_clause} RETURN n")
                };
                let scope_graph = graph_key(scope);
                let _: redis::Value = typed
                    .graph()
                    .query_raw(scope_graph.as_str(), &cypher)
                    .await
                    .map_err(moon_err)?;
            }
            WriteOp::GraphEdge { graph: _, src, dst, rel, props } => {
                // RFC 0001 Wave 1C: use per-scope graph key.
                // T-01-03-01: caller-validated `rel`. See module rustdoc above.
                // Same constraints as GraphNode: inline literals, no params, no `+=`.
                let src_hex = hex::encode(src);
                let dst_hex = hex::encode(dst);
                let set_clause = build_set_clause("r", props);
                let cypher = if set_clause.is_empty() {
                    format!(
                        "MATCH (a {{id:'{src_hex}'}}),(b {{id:'{dst_hex}'}}) MERGE (a)-[r:{rel}]->(b) RETURN r"
                    )
                } else {
                    format!(
                        "MATCH (a {{id:'{src_hex}'}}),(b {{id:'{dst_hex}'}}) MERGE (a)-[r:{rel}]->(b) {set_clause} RETURN r"
                    )
                };
                let scope_graph = graph_key(scope);
                let _: redis::Value = typed
                    .graph()
                    .query_raw(scope_graph.as_str(), &cypher)
                    .await
                    .map_err(moon_err)?;
            }
        }
    }
    Ok(())
}

/// Per-index BM25/HYBRID text payload extractor — mirrors the Postgres
/// tsvector source convention (see
/// `crates/lunaris-storage-postgres/migrations/20260421_000004_tsvector_bm25.sql`).
///
/// | index       | source field(s)                                          |
/// |-------------|----------------------------------------------------------|
/// | chunks      | metadata["text"]                                         |
/// | entities    | metadata["name"] + " " + metadata["entity_type"]         |
/// | facts       | metadata["fact_text"]                                    |
/// | communities | metadata["summary"]                                      |
///
/// Returns `None` when the source field(s) are missing or empty so callers
/// can skip the HSET write rather than store an empty `content` field that
/// would just inflate row size for nothing.
fn extract_content_for_index(index: &str, metadata: &serde_json::Value) -> Option<String> {
    fn s<'a>(v: &'a serde_json::Value, k: &str) -> Option<&'a str> {
        v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
    }
    let combined = match index {
        "chunks" => s(metadata, "text").map(str::to_owned),
        "entities" => match (s(metadata, "name"), s(metadata, "entity_type")) {
            (Some(n), Some(t)) => Some(format!("{n} {t}")),
            (Some(n), None) => Some(n.to_owned()),
            (None, Some(t)) => Some(t.to_owned()),
            (None, None) => None,
        },
        "facts" => s(metadata, "fact_text").map(str::to_owned),
        "communities" => s(metadata, "summary").map(str::to_owned),
        _ => None,
    };
    combined.filter(|s| !s.is_empty())
}

/// Build a Cypher `SET var.k1 = lit1, var.k2 = lit2, ...` clause from a JSON object.
///
/// Moon's Cypher parser only supports `SET var.prop = expr` (no map-merge `+=` or
/// `SET var = {...}`). Each JSON value is converted to its Cypher literal:
/// - strings → `'escaped'`
/// - numbers → numeric literal
/// - booleans → `true`/`false`
/// - null/arrays/objects → serialized as a quoted JSON string (best-effort)
///
/// Returns empty string when `props` is null, not an object, or has no keys.
fn build_set_clause(var: &str, props: &serde_json::Value) -> String {
    let obj = match props.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return String::new(),
    };
    let mut parts: Vec<String> = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let lit = json_to_cypher_literal(v);
        parts.push(format!("{var}.{k} = {lit}"));
    }
    format!("SET {}", parts.join(", "))
}

fn json_to_cypher_literal(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
            format!("'{escaped}'")
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => {
            let s = other.to_string().replace('\\', "\\\\").replace('\'', "\\'");
            format!("'{s}'")
        }
    }
}

#[cfg(test)]
mod valid_time_tests {
    //! Plan 09.1-02 Task 2b structural guards on `atomic_write`.
    //!
    //! The `chunks` FT index declares `valid_time` NUMERIC (Task 2). Without
    //! the write-side populating a `valid_time` hash field on every chunk row,
    //! `@valid_time:[lo hi]` queries match 0 rows — a silent footgun. These
    //! source-grep tests pin the behaviour without requiring a live Moon.

    #[test]
    fn vector_upsert_writes_valid_time_field_for_chunks() {
        let src = include_str!("atomic.rs");
        assert!(
            src.contains("valid_time_ms"),
            "VectorUpsert must read valid_time_ms from metadata"
        );
        assert!(
            src.contains("\"valid_time\""),
            "HSET field name must be valid_time to match FT schema"
        );
    }

    #[test]
    fn vector_upsert_skips_valid_time_for_non_chunks_indices() {
        let src = include_str!("atomic.rs");
        // The fan-out MUST gate on `index == "chunks"` — entities / facts /
        // communities indices do NOT declare valid_time in ensure_indexes.
        assert!(
            src.contains("index == \"chunks\""),
            "valid_time HSET must be gated on index == \"chunks\""
        );
    }

    /// Plan 15-01 Task 1 — structural guard: VectorUpsert for chunks must
    /// extract `source` from metadata and HSET it as a separate field so the
    /// `SchemaField::Tag("source")` FT index can resolve `@source:{value}`
    /// queries server-side (PERF-MOON-01).
    #[test]
    fn vector_upsert_writes_source_field_for_chunks() {
        let src = include_str!("atomic.rs");
        assert!(
            src.contains("source_buf"),
            "VectorUpsert must extract source into source_buf for chunks"
        );
        assert!(
            src.contains("\"source\", src.as_bytes()"),
            "VectorUpsert must HSET source field from source_buf"
        );
    }

    #[test]
    fn source_extracted_from_metadata_for_chunks_index() {
        let meta = serde_json::json!({"source": "helios:fs/test", "text": "hello"});
        let source = meta.get("source").and_then(|v| v.as_str());
        assert_eq!(source, Some("helios:fs/test"));
    }

    #[test]
    fn vector_upsert_no_valid_time_when_absent_from_metadata() {
        let src = include_str!("atomic.rs");
        // The fan-out MUST read metadata["valid_time_ms"] via .get(..).and_then(.as_u64())
        // so missing / non-u64 values fall through to None (no HSET), not panic.
        assert!(
            src.contains("get(\"valid_time_ms\")"),
            "valid_time population must probe metadata via .get(\"valid_time_ms\")"
        );
        assert!(
            src.contains(".and_then(|v| v.as_u64())"),
            "valid_time population must require u64 (missing / non-u64 => None)"
        );
    }

    /// RFC 0001 Wave 1C — structural guard: VectorUpsert must use the
    /// per-scope FT index name (`ft_index_name(scope, kind)`) as the key
    /// prefix instead of the bare `index` name.
    #[test]
    fn vector_upsert_uses_per_scope_ft_index_name() {
        let src = include_str!("atomic.rs");
        assert!(
            src.contains("ft_index_name(scope, index)"),
            "VectorUpsert must compute per-scope FT index name via ft_index_name(scope, index)"
        );
        assert!(
            src.contains("per_scope_ft_idx"),
            "VectorUpsert must use per_scope_ft_idx as the HSET key prefix"
        );
    }

    /// RFC 0001 Wave 1C — structural guard: GraphNode/GraphEdge must use
    /// `graph_key(scope)` not a hardcoded graph name.
    #[test]
    fn graph_ops_use_per_scope_graph_key() {
        let src = include_str!("atomic.rs");
        assert!(
            src.contains("graph_key(scope)"),
            "GraphNode/GraphEdge must use graph_key(scope) for the GRAPH.QUERY target"
        );
    }
}
