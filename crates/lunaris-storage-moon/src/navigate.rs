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
//! `vector_search` (see `vector::decode_key`) — but tried against EVERY FT
//! index kind this scope owns, not just the seed index. A graph hop can land
//! on a node registered against a different index than the KNN seed (KG-RAG
//! facts-as-graph-nodes: an `entities` seed can hop to a `Fact` node whose
//! `_key` carries the `facts` prefix) — only a genuinely foreign SCOPE's
//! index name fails every prefix and gets dropped.
//!
//! `FT.NAVIGATE DECAY` has no time-weight slot (the server fixes
//! `time_weight = 1.0`), so only the λ of a `GraphDecay` is sent — documented
//! on `NavigateSpec`.

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::{NavigateHit, NavigateSpec};

use crate::client::{MoonClient, redis_err};
use crate::keyspace::ft_index_name;
use crate::vector::{SCOPE_VECTOR_INDEX_KINDS, decode_key};

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

    parse_ft_navigate(reply, scope)
}

/// Every per-scope FT index prefix a graph-expanded hop can legally land on.
///
/// KG-RAG facts-as-graph-nodes: a hop from an `entities` KNN seed can reach a
/// `Fact` node (registered via `GRAPH.ADDNODE ... _key facts:<hex>`), whose
/// key carries a DIFFERENT index prefix than the seed. The single-prefix
/// `decode_key(raw, seed_index)` scope-isolation check (designed only to
/// reject a genuinely foreign SCOPE's keys, never exercised cross-INDEX
/// before this) would silently drop every such hit as "foreign-prefix".
/// Trying every kind this scope owns keeps the same scope-isolation guarantee
/// (a different scope's index name never matches) while accepting any kind
/// within THIS scope's graph.
fn scope_index_prefixes(scope: &Scope) -> [String; SCOPE_VECTOR_INDEX_KINDS.len()] {
    std::array::from_fn(|i| ft_index_name(scope, SCOPE_VECTOR_INDEX_KINDS[i]))
}

fn parse_ft_navigate(v: redis::Value, scope: &Scope) -> Result<Vec<NavigateHit>, StorageError> {
    let prefixes = scope_index_prefixes(scope);
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
        let Some(id_bytes) = prefixes.iter().find_map(|prefix| decode_key(&raw_key, prefix)) else {
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

    fn dev_scope() -> Scope {
        Scope::new("dev").unwrap()
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
        let hits = parse_ft_navigate(v, &dev_scope()).unwrap();
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
        let hits = parse_ft_navigate(v, &dev_scope()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hop_depth, 0);
        assert!((hits[0].final_score - 0.5).abs() < 1e-6, "vec_score is the fallback final score");
    }

    #[test]
    fn parse_navigate_drops_foreign_prefix_keys() {
        let v = redis::Value::Array(vec![
            redis::Value::Int(1),
            bulk("otherscope_idx:0101"),
            redis::Value::Array(vec![]),
        ]);
        let hits = parse_ft_navigate(v, &dev_scope()).unwrap();
        assert!(hits.is_empty(), "a key from a different scope's index must still be dropped");
    }

    /// KG-RAG facts-as-graph-nodes: a hop from an `entities` KNN seed can
    /// land on a `Fact` graph node, whose `_key` carries the `facts` index
    /// prefix — a DIFFERENT prefix than the seed. This must NOT be treated
    /// as foreign — it's the same scope, just a different node kind.
    #[test]
    fn parse_navigate_accepts_cross_index_hop_within_same_scope() {
        let entities_idx = "lunaris_dev_entities_idx";
        let facts_idx = "lunaris_dev_facts_idx";
        let seed_id = hex::encode([1u8; 16]);
        let hop_id = hex::encode([2u8; 16]);
        let v = redis::Value::Array(vec![
            redis::Value::Int(2),
            bulk(&format!("{entities_idx}:{seed_id}")),
            redis::Value::Array(vec![
                bulk("__hop_depth"),
                bulk("0"),
                bulk("__final_score"),
                bulk("0.9"),
            ]),
            bulk(&format!("{facts_idx}:{hop_id}")),
            redis::Value::Array(vec![
                bulk("__hop_depth"),
                bulk("1"),
                bulk("__final_score"),
                bulk("0.5"),
            ]),
        ]);
        let hits = parse_ft_navigate(v, &dev_scope()).unwrap();
        assert_eq!(hits.len(), 2, "the facts-index hop must survive, not be dropped as foreign");
        assert_eq!(hits[0].id, vec![1u8; 16]);
        assert_eq!(hits[1].id, vec![2u8; 16]);
        assert_eq!(hits[1].hop_depth, 1);
    }
}
