//! `.and / .or / .then` combinators.
//!
//! All three preserve the upstream operators as `Box<dyn Retriever>` so the
//! downstream `fuse_rrf` operator (in `fuse.rs`) can introspect the tree
//! shape AND so concurrent branches can run via `tokio::join!`.
//!
//! ## Semantics
//!
//! - `.and(A, B)` — runs A and B concurrently, **concatenates** their results
//!   (per-source ranking preserved via `RawHit.source_op`). Downstream
//!   `fuse_rrf` groups by `source_op` to fold the per-branch rankings.
//! - `.or(A, B)` — runs A and B concurrently, **unions** their results by id;
//!   on duplicate id, takes the max score. The merged hit's `source_op` is
//!   the producer of the higher-scoring instance.
//! - `.then(A, B)` — runs A first, then passes A's hit ids as a
//!   `Filter::Or(Filter::Eq{id, …})` to B (B's results are the only ones
//!   returned). Use this for "narrow then re-rank" pipelines.

use std::any::Any;
use std::collections::HashMap;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_core::storage::types::Filter;

use super::{QueryContext, Retriever};
use crate::types::RawHit;

/// `.and(A, B)` — concurrent fan-out, concatenated output (with per-source tag preserved).
pub struct AndRetriever {
    pub(crate) left: Box<dyn Retriever>,
    pub(crate) right: Box<dyn Retriever>,
}

impl AndRetriever {
    pub fn new(left: Box<dyn Retriever>, right: Box<dyn Retriever>) -> Self {
        Self { left, right }
    }

    pub fn fuse_rrf(self, k: u32) -> super::fuse::FuseRrfRetriever {
        super::fuse::FuseRrfRetriever::new(Box::new(self), k as usize)
    }

    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }
}

#[async_trait]
impl Retriever for AndRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let (left_res, right_res) = tokio::join!(self.left.retrieve(ctx), self.right.retrieve(ctx));
        let mut out = left_res?;
        out.extend(right_res?);
        Ok(out)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `.or(A, B)` — concurrent fan-out, unioned output (by id, max score).
pub struct OrRetriever {
    pub(crate) left: Box<dyn Retriever>,
    pub(crate) right: Box<dyn Retriever>,
}

impl OrRetriever {
    pub fn new(left: Box<dyn Retriever>, right: Box<dyn Retriever>) -> Self {
        Self { left, right }
    }

    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }
}

#[async_trait]
impl Retriever for OrRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let (left_res, right_res) = tokio::join!(self.left.retrieve(ctx), self.right.retrieve(ctx));
        let mut by_id: HashMap<Vec<u8>, RawHit> = HashMap::new();
        for h in left_res?.into_iter().chain(right_res?) {
            by_id
                .entry(h.id.clone())
                .and_modify(|existing| {
                    if h.score > existing.score {
                        *existing = h.clone();
                    }
                })
                .or_insert(h);
        }
        let mut out: Vec<RawHit> = by_id.into_values().collect();
        // Stable order by descending score so .top(n) takes the top-N.
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `.then(A, B)` — sequential narrow: A's ids constrain B via `Filter::Or(Eq{id})`.
pub struct ThenRetriever {
    pub(crate) first: Box<dyn Retriever>,
    pub(crate) second: Box<dyn Retriever>,
}

impl ThenRetriever {
    pub fn new(first: Box<dyn Retriever>, second: Box<dyn Retriever>) -> Self {
        Self { first, second }
    }

    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }
}

#[async_trait]
impl Retriever for ThenRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        let firsts = self.first.retrieve(ctx).await?;
        if firsts.is_empty() {
            return Ok(Vec::new());
        }
        // Build Filter::Or([Filter::Eq{field:"id", value:<id-as-string>}, ...])
        let id_filter = Filter::Or(
            firsts
                .iter()
                .map(|h| Filter::Eq {
                    field: "id".to_string(),
                    value: serde_json::Value::String(String::from_utf8_lossy(&h.id).into_owned()),
                })
                .collect(),
        );

        // Compose with any existing filter — Filter::And([existing, id_filter])
        let new_filter = match &ctx.query.filter {
            Some(existing) => Filter::And(vec![existing.clone(), id_filter]),
            None => id_filter,
        };

        // We need a fresh ctx with the new filter. Build one quickly: the
        // OnceCell embedding cache would be reset, so we just clone the
        // user query and add the filter, building a new QueryContext.
        let mut narrowed_query = ctx.query.clone();
        narrowed_query.filter = Some(new_filter);

        let narrowed_ctx = QueryContext {
            query: narrowed_query,
            embedder: ctx.embedder.clone(),
            storage: ctx.storage.clone(),
            keyword: ctx.keyword.clone(),
            // Re-init the OnceCell — we can't share the cell across contexts
            // (different `&self` borrows). The cost is one extra embedder
            // call when `then(Vector, Vector)` runs back-to-back; acceptable
            // in v0 because `then(Vector, Vector)` is rare in practice.
            query_embedding: tokio::sync::OnceCell::new(),
            moon_storage: ctx.moon_storage.clone(),
        };

        self.second.retrieve(&narrowed_ctx).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
