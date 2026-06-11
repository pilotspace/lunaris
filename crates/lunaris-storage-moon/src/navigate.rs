//! `vector_navigate` — Moon `FT.NAVIGATE` graph-expanded KNN over per-scope
//! indices (ADD task `ft-navigate-recall`, contract v1).
//!
//! Wire shape (raw RESP — the typed SDK `navigate` wrapper has no DECAY slot):
//!
//! ```text
//! FT.NAVIGATE {ft_index_name(scope,index)} "*=>[KNN {k} @vec $v]"
//!             HOPS {hops} [HOP_PENALTY {p}] [DECAY {λ}]
//!             PARAMS 2 v <f32-le blob>
//! ```
//!
//! Reply mirrors FT.SEARCH — `[count, key, [field, value, …], …]` — with
//! `__vec_score`, `__hop_depth`, and `__final_score` fields per hit. Keys are
//! decoded with the same `{ft_index}:{hex}` prefix-strip rule as
//! `vector_search` (see `vector::decode_key`).
//!
//! `FT.NAVIGATE DECAY` has no time-weight slot (the server fixes
//! `time_weight = 1.0`), so only the λ of a `GraphDecay` is sent — documented
//! on `NavigateSpec`.

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{NavigateHit, NavigateSpec};

use crate::client::{MoonClient, redis_err};
use crate::keyspace::ft_index_name;
use crate::vector::decode_key;

pub(crate) async fn vector_navigate(
    c: &MoonClient,
    scope: &Scope,
    index: &str,
    query: &[f32],
    k: usize,
    spec: &NavigateSpec,
) -> Result<Vec<NavigateHit>, StorageError> {
    let per_scope_index = ft_index_name(scope, index);

    let mut qbytes = Vec::with_capacity(query.len() * 4);
    for f in query {
        qbytes.extend_from_slice(&f.to_le_bytes());
    }
    let knn_query = format!("*=>[KNN {k} @vec $v]");

    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let mut cmd = redis::cmd("FT.NAVIGATE");
    cmd.arg(per_scope_index.as_str()).arg(&knn_query).arg("HOPS").arg(spec.hops());
    if let Some(p) = spec.hop_penalty() {
        cmd.arg("HOP_PENALTY").arg(p);
    }
    if let Some(d) = spec.decay() {
        cmd.arg("DECAY").arg(d.lambda());
    }
    cmd.arg("PARAMS").arg(2).arg("v").arg(qbytes.as_slice());
    let reply: redis::Value = cmd.query_async(raw_conn).await.map_err(redis_err)?;

    parse_ft_navigate(reply, &per_scope_index)
}

fn parse_ft_navigate(v: redis::Value, ft_index: &str) -> Result<Vec<NavigateHit>, StorageError> {
    let arr = match v {
        redis::Value::Array(a) => a,
        other => {
            return Err(StorageError::Backend(format!("FT.NAVIGATE unexpected reply: {other:?}")));
        }
    };
    let mut hits = Vec::new();
    let mut iter = arr.into_iter();
    let _count = iter.next();
    while let (Some(id), Some(fields)) = (iter.next(), iter.next()) {
        let raw_key = match id {
            redis::Value::BulkString(b) => b,
            redis::Value::SimpleString(s) => s.into_bytes(),
            _ => continue,
        };
        let Some(id_bytes) = decode_key(&raw_key, ft_index) else {
            continue;
        };
        let mut vec_score = 0.0f32;
        let mut hop_depth = 0u32;
        let mut final_score: Option<f32> = None;
        if let redis::Value::Array(kv) = fields {
            let mut it = kv.into_iter();
            while let (Some(k), Some(val)) = (it.next(), it.next()) {
                let key = match k {
                    redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                    redis::Value::SimpleString(s) => s,
                    _ => continue,
                };
                let text = match val {
                    redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                    redis::Value::SimpleString(s) => s,
                    redis::Value::Int(n) => n.to_string(),
                    _ => continue,
                };
                match key.as_str() {
                    "__vec_score" => vec_score = text.parse().unwrap_or(0.0),
                    "__hop_depth" => hop_depth = text.parse().unwrap_or(0),
                    "__final_score" => final_score = text.parse().ok(),
                    _ => {}
                }
            }
        }
        // KNN-only fallback replies (no graph for the seeds) omit
        // __final_score — the vector distance IS the final score there.
        hits.push(NavigateHit {
            id: id_bytes,
            vec_score,
            hop_depth,
            final_score: final_score.unwrap_or(vec_score),
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> redis::Value {
        redis::Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn parse_navigate_reply_with_hop_metadata() {
        let idx = "lunaris_dev_entities_idx";
        let id_a = hex::encode([1u8; 16]);
        let id_b = hex::encode([2u8; 16]);
        let v = redis::Value::Array(vec![
            redis::Value::Int(2),
            bulk(&format!("{idx}:{id_a}")),
            redis::Value::Array(vec![
                bulk("__vec_score"),
                bulk("0.01"),
                bulk("__hop_depth"),
                bulk("0"),
                bulk("__final_score"),
                bulk("0.01"),
            ]),
            bulk(&format!("{idx}:{id_b}")),
            redis::Value::Array(vec![
                bulk("__vec_score"),
                bulk("0"),
                bulk("__hop_depth"),
                bulk("2"),
                bulk("__final_score"),
                bulk("0.2"),
            ]),
        ]);
        let hits = parse_ft_navigate(v, idx).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, vec![1u8; 16]);
        assert_eq!(hits[0].hop_depth, 0);
        assert_eq!(hits[1].id, vec![2u8; 16]);
        assert_eq!(hits[1].hop_depth, 2);
        assert!((hits[1].final_score - 0.2).abs() < 1e-6);
    }

    /// KNN-only fallback (annotate_knn_only) carries no __final_score —
    /// the vector distance must flow through as the final score.
    #[test]
    fn parse_navigate_knn_only_reply_defaults_final_score() {
        let idx = "lunaris_dev_chunks_idx";
        let id = hex::encode([3u8; 16]);
        let v = redis::Value::Array(vec![
            redis::Value::Int(1),
            bulk(&format!("{idx}:{id}")),
            redis::Value::Array(vec![
                bulk("__vec_score"),
                bulk("0.5"),
                bulk("__hop_depth"),
                bulk("0"),
            ]),
        ]);
        let hits = parse_ft_navigate(v, idx).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hop_depth, 0);
        assert!((hits[0].final_score - 0.5).abs() < 1e-6, "vec_score is the fallback final score");
    }

    #[test]
    fn parse_navigate_drops_foreign_prefix_keys() {
        let idx = "lunaris_dev_entities_idx";
        let v = redis::Value::Array(vec![
            redis::Value::Int(1),
            bulk("otherscope_idx:0101"),
            redis::Value::Array(vec![]),
        ]);
        let hits = parse_ft_navigate(v, idx).unwrap();
        assert!(hits.is_empty(), "foreign-prefix keys must be dropped");
    }
}
