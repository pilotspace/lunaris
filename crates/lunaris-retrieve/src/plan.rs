//! F14 — build a real retriever tree from a JSON operator plan.
//!
//! The Python and TypeScript SDKs expose a composable DSL (`.and_()`,
//! `.fuse_rrf()`, `.top()`) but their FFI historically carried a single flat
//! `{index, k}` plan. Anything richer was collapsed to one leg, so both SDKs
//! refused those plans rather than answer a different question than the one
//! written. That refusal was right; this module removes the need for it.
//!
//! Both SDKs marshal their operator tree into the JSON shape below and call
//! [`retriever_from_json`], so **one** parser decides what a plan means and
//! the shape a caller writes is the shape the engine runs.
//!
//! ```text
//! {"op":"vector",   "index":"chunks", "k":30}
//! {"op":"keyword",  "index":"chunks", "k":30}
//! {"op":"graph",    "seeds":[<seed>,…], "hops":2}
//! {"op":"and",      "left":<node>, "right":<node>}
//! {"op":"fuse_rrf", "k":60, "child":<node>}
//! {"op":"top",      "n":5,  "child":<node>}
//! ```
//!
//! A `<seed>` is either the 32-char lowercase hex an [`EntityId`] renders as
//! (what the engine emits) or `{"name":"Alice","type":"Person"}` with an
//! optional `"confidence"` (what a human writes). Both resolve through
//! [`EntityId::from_name_and_type`] / [`EntityId::from_hex`] to the same
//! anchor.
//!
//! ## Errors are the contract
//!
//! Every unrecognized op, missing branch and malformed seed is an
//! [`PlanError`], never a skip and never a default. A parser that quietly
//! drops a node it does not understand rebuilds the exact defect this module
//! exists to remove: a plan that runs is not the plan that was written, and
//! the caller gets a plausible list of hits with no indication of the swap.

use lunaris_extract::types::EntityId;
use serde_json::Value;

use crate::operators::Retriever;
use crate::operators::combinators::AndRetriever;
use crate::operators::fuse::FuseRrfRetriever;
use crate::operators::graph::Graph;
use crate::operators::keyword::Keyword;
use crate::operators::modifiers::TopRetriever;
use crate::operators::vector::Vector;

/// Why a JSON plan could not be turned into a retriever.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan node is not a JSON object: {0}")]
    NotAnObject(String),
    #[error("plan node has no `op` field: {0}")]
    MissingOp(String),
    #[error(
        "unrecognized plan op `{0}` — the SDK plan parser does not build this operator, and \
         skipping it would run a different plan than the one written"
    )]
    UnknownOp(String),
    #[error("plan op `{op}` is missing required field `{field}`")]
    MissingField { op: String, field: &'static str },
    #[error("plan op `{op}` field `{field}` has the wrong type (wanted {wanted})")]
    BadField { op: String, field: &'static str, wanted: &'static str },
    #[error("graph seed {index} is neither 32-char hex nor a {{\"name\",\"type\"}} pair: {seed}")]
    BadSeed { index: usize, seed: String },
}

type Built = Result<Box<dyn Retriever>, PlanError>;

/// Build a retriever tree from a JSON plan node. See the module docs for the
/// accepted shape.
pub fn retriever_from_json(node: &Value) -> Built {
    let obj = node.as_object().ok_or_else(|| PlanError::NotAnObject(node.to_string()))?;
    let op = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| PlanError::MissingOp(node.to_string()))?;

    match op {
        "vector" => {
            Ok(Box::new(Vector::new(str_field(node, op, "index")?, usize_field(node, op, "k")?)))
        }
        "keyword" => {
            Ok(Box::new(Keyword::bm25(str_field(node, op, "index")?, usize_field(node, op, "k")?)))
        }
        "graph" => {
            let seeds = seeds_field(node, op)?;
            let hops = usize_field(node, op, "hops")?;
            Ok(Box::new(Graph::anchored(seeds, hops)))
        }
        "and" => Ok(Box::new(AndRetriever::new(
            retriever_from_json(child_field(node, op, "left")?)?,
            retriever_from_json(child_field(node, op, "right")?)?,
        ))),
        "fuse_rrf" => Ok(Box::new(FuseRrfRetriever::new(
            retriever_from_json(child_field(node, op, "child")?)?,
            usize_field(node, op, "k")?,
        ))),
        "top" => Ok(Box::new(TopRetriever::new(
            retriever_from_json(child_field(node, op, "child")?)?,
            usize_field(node, op, "n")?,
        ))),
        other => Err(PlanError::UnknownOp(other.to_string())),
    }
}

/// The hex `EntityId`s a graph root anchors on, or `None` when the root is
/// not a [`Graph`]. Exists so a caller can prove the seeds it wrote are the
/// seeds that were built — `plan_repr` deliberately renders only the seed
/// COUNT, since the ids themselves are unbounded and belong in a trace, not
/// in a plan string that gets compared for equality.
pub fn seed_hex(r: &dyn Retriever) -> Option<Vec<String>> {
    r.as_any()
        .downcast_ref::<Graph>()
        .map(|g| g.seeds.iter().map(|(id, _)| id.to_string()).collect())
}

fn field<'a>(node: &'a Value, op: &str, name: &'static str) -> Result<&'a Value, PlanError> {
    node.get(name).ok_or_else(|| PlanError::MissingField { op: op.to_string(), field: name })
}

fn str_field<'a>(node: &'a Value, op: &str, name: &'static str) -> Result<&'a str, PlanError> {
    field(node, op, name)?.as_str().ok_or_else(|| PlanError::BadField {
        op: op.to_string(),
        field: name,
        wanted: "a string",
    })
}

fn usize_field(node: &Value, op: &str, name: &'static str) -> Result<usize, PlanError> {
    field(node, op, name)?.as_u64().map(|n| n as usize).ok_or_else(|| PlanError::BadField {
        op: op.to_string(),
        field: name,
        wanted: "a non-negative integer",
    })
}

fn child_field<'a>(node: &'a Value, op: &str, name: &'static str) -> Result<&'a Value, PlanError> {
    field(node, op, name)
}

/// Parse `"seeds"` into the `(EntityId, confidence)` pairs `Graph::anchored`
/// takes. A seed is a 32-char hex id or a `{"name","type"[,"confidence"]}`
/// object; anything else is an error naming the offending index, because a
/// dropped seed is an anchor the traversal silently never started from.
fn seeds_field(node: &Value, op: &str) -> Result<Vec<(EntityId, f32)>, PlanError> {
    let arr = field(node, op, "seeds")?.as_array().ok_or_else(|| PlanError::BadField {
        op: op.to_string(),
        field: "seeds",
        wanted: "an array",
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (index, seed) in arr.iter().enumerate() {
        let bad = || PlanError::BadSeed { index, seed: seed.to_string() };
        if let Some(s) = seed.as_str() {
            out.push((EntityId::from_hex(s).ok_or_else(bad)?, 1.0));
            continue;
        }
        let name = seed.get("name").and_then(Value::as_str).ok_or_else(bad)?;
        let ty = seed.get("type").and_then(Value::as_str).ok_or_else(bad)?;
        let conf = seed.get("confidence").and_then(Value::as_f64).unwrap_or(1.0) as f32;
        out.push((EntityId::from_name_and_type(name, ty), conf));
    }
    Ok(out)
}
