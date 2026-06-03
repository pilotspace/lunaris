//! `Tree` operator — RAPTOR hierarchical retrieval over the `communities` index.
//!
//! ## What it does
//!
//! 1. Embeds the query (shared via [`super::QueryContext::embed_once`]).
//! 2. Vector-searches the `"communities"` FT index for the `k` closest
//!    community summaries.
//! 3. For each community hit, reads the [`lunaris_core::Community`] KV row
//!    (via `StoragePort::read_as_of`) to get `Community.members`.
//! 4. Partitions `members` into **leaf chunks** vs **sub-communities** by
//!    probing: if `community_key(scope, id)` exists in the KV store, it's a
//!    sub-community; otherwise treat it as a leaf chunk.
//! 5. Emits `RawHit { id: chunk_id_bytes, source_op: SourceOp::Tree }` for
//!    every resolved leaf chunk so downstream `hydrate()` can resolve the full
//!    chunk text (chunk keys are looked up by `hydrate` in the normal path).
//!
//! ## Why this matters (RAPTOR discrimination)
//!
//! Flat chunk retrieval at small `k` can only return individual chunks. A
//! whole-document or multi-hop question whose answer spans several chunks
//! benefits from the community summary node, which *semantically aggregates*
//! those chunks. The `Tree` operator finds the nearest community summary node
//! first, then exposes ALL leaf chunks under it — including chunks that would
//! be ranked outside the flat search's top-k budget.
//!
//! ## BFS depth and DoS guard
//!
//! `Tree::new(index, k)` performs **one level of descent** (community →
//! direct members) by default. `Tree::with_depth(d)` expands BFS up to `d`
//! levels deep (capped at [`MAX_TREE_DEPTH`]).  Each level fetches only the
//! community rows from the KV store (no new vector search is issued), so
//! latency scales with depth × fan-out, not with index size.
//!
//! ## Seam constraints
//!
//! Per the DAG execution context, `operators/mod.rs` and `builder.rs` are
//! **orchestrator-owned seams**. The `mod tree;` declaration and `.tree(...)`
//! builder method are returned as additive snippets — the orchestrator applies
//! them. This file is a standalone new file, safe to commit on the worktree
//! branch.

use std::any::Any;
use std::collections::{HashSet, VecDeque};

use async_trait::async_trait;
use lunaris_core::keyspace::{chunk_key, community_key};
use lunaris_core::{Community, HlcClock, LunarisError};

use super::{QueryContext, Retriever, clamp_k};
use crate::types::{RawHit, SourceOp};

/// Hard cap on BFS descent depth to prevent DoS via a degenerate community tree
/// with very large fan-out at each level.
pub const MAX_TREE_DEPTH: usize = 4;

/// Default BFS descent depth when not specified. One level means: find the
/// nearest communities, then collect their direct member chunks. Deeper levels
/// recurse into sub-communities.
pub const DEFAULT_TREE_DEPTH: usize = 1;

/// RAPTOR tree retrieval operator.
///
/// Searches the `communities` vector index, then descends the community→members
/// hierarchy to collect leaf-chunk IDs that `hydrate()` can resolve into `Hit`s.
///
/// Construction is fluent. Pass it to [`crate::RetrievalBuilder::with_root`] or
/// compose with other operators via `.and()` / `.or()` / `.fuse_rrf()`.
///
/// ```ignore
/// use lunaris_retrieve::{RetrievalBuilder, Query};
/// use lunaris_retrieve::operators::tree::Tree;
///
/// let hits = builder
///     .with_root(Tree::new("communities", 5))
///     .execute(Query::text("What are the main themes across both reports?"))
///     .await?;
/// ```
#[derive(Clone, Debug)]
#[must_use = "Tree is a query node — pass it to RetrievalBuilder::with_root() or chain via .and/.or/.fuse_rrf, otherwise it never executes"]
pub struct Tree {
    /// Vector index to search for community summaries. Typically `"communities"`.
    pub index: String,
    /// Number of top community nodes to retrieve from the vector index.
    pub k: usize,
    /// BFS descent depth: 1 = direct members only, 2 = members + their members, etc.
    pub depth: usize,
}

impl Tree {
    /// Build a `Tree` operator targeting the given community index with `k`
    /// top-level nodes and the default descent depth of [`DEFAULT_TREE_DEPTH`].
    ///
    /// `k` is clamped to [`super::MAX_K`].
    pub fn new(index: impl Into<String>, k: usize) -> Self {
        Self { index: index.into(), k: clamp_k(k), depth: DEFAULT_TREE_DEPTH }
    }

    /// Override the BFS descent depth (clamped to [`MAX_TREE_DEPTH`]).
    ///
    /// `depth = 1` (default): collect direct `Community.members` as leaf chunks.
    /// `depth = 2`: also expand sub-community members one additional level, etc.
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth.clamp(1, MAX_TREE_DEPTH);
        self
    }

    /// Convenience: wrap into the standard combinator builder.
    pub fn and<R: Retriever + 'static>(self, other: R) -> super::combinators::AndRetriever {
        super::combinators::AndRetriever::new(Box::new(self), Box::new(other))
    }

    pub fn or<R: Retriever + 'static>(self, other: R) -> super::combinators::OrRetriever {
        super::combinators::OrRetriever::new(Box::new(self), Box::new(other))
    }

    pub fn then<R: Retriever + 'static>(self, other: R) -> super::combinators::ThenRetriever {
        super::combinators::ThenRetriever::new(Box::new(self), Box::new(other))
    }

    /// Cap the final result set after the operator tree resolves.
    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }
}

#[async_trait]
impl Retriever for Tree {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        // Step 1: embed query (shared via OnceCell — no re-embed if chained with Vector).
        let q_emb = ctx.embed_once().await?;

        // Step 2: vector-search the communities index for the k closest summary nodes.
        //
        // Graceful-empty for missing index: if no data has been ingested under this scope
        // the communities FT index may not yet exist (Moon creates indexes lazily on first
        // VectorUpsert). A missing-index error from the backend surfaces as
        // `StorageError::Backend` containing "Index name" — treat that as "no communities"
        // rather than propagating a hard error to the caller. The Vector operator uses the
        // same degradation pattern: sparse indexes are a normal production state when RAPTOR
        // has not yet built any community nodes for a freshly-created scope.
        let community_hits = match ctx
            .storage
            .vector_search(
                &ctx.scope,
                &self.index,
                &q_emb,
                self.k,
                ctx.query.filter.as_ref(),
                ctx.query.as_of,
                false,
            )
            .await
        {
            Ok(hits) => hits,
            // Graceful-empty for missing index: `vector_search` returns `StorageError`
            // (not `LunarisError`). Moon surfaces a missing FT index as
            // `StorageError::Backend("moon: redis error: \"Unknown\": Index name")`.
            // Treat this as "scope has no ingested communities" → return empty rather
            // than propagating a hard error. This is the normal state for a freshly
            // created scope before any ingest has run.
            Err(lunaris_core::StorageError::Backend(ref msg)) if msg.contains("Index name") => {
                tracing::debug!(
                    scope = %ctx.scope,
                    index = %self.index,
                    "Tree: communities index not found (scope has no ingested communities); returning empty"
                );
                return Ok(vec![]);
            }
            Err(e) => return Err(LunarisError::Storage(e)),
        };

        if community_hits.is_empty() {
            return Ok(vec![]);
        }

        // Use a live clock for hydration when as_of is not set.
        let live_clock = HlcClock::new(0);
        let snapshot = ctx.query.as_of.unwrap_or_else(|| live_clock.tick());

        // Step 3–5: BFS descent from matched community nodes → leaf chunk IDs.
        //
        // Invariant: we never emit a community ULID as a RawHit.id — those are NOT
        // chunk keys and hydrate() would silently drop them. We only emit chunk ULIDs.
        let mut leaf_ids: Vec<(Vec<u8>, f32)> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();

        for community_hit in community_hits {
            // `community_hit.id` is the ULID bytes of the community node.
            let community_id_bytes = &community_hit.id;
            let community_ulid = match ulid_from_bytes(community_id_bytes) {
                Some(u) => u,
                None => continue,
            };

            // BFS queue: (ulid_bytes, score_from_parent, current_depth)
            let mut queue: VecDeque<(ulid::Ulid, f32, usize)> = VecDeque::new();
            queue.push_back((community_ulid, community_hit.score, 0));

            while let Some((ulid, score, depth)) = queue.pop_front() {
                // Read the community KV row.
                let key = community_key(&ctx.scope, ulid);
                let row = ctx.storage.read_as_of(&ctx.scope, &key, snapshot).await?;

                let community: Community = match row
                    .and_then(|r| serde_json::from_slice::<Community>(&r.value).ok())
                {
                    Some(c) => c,
                    None => {
                        // KV row missing or malformed — treat this id as a leaf chunk
                        // (it may be a chunk id that was stored in members as a peer;
                        // the BFS only starts from valid community hits so this is
                        // normally a data-integrity edge case, not the hot path).
                        let id_bytes = ulid.to_bytes().to_vec();
                        if seen.insert(id_bytes.clone()) {
                            // Verify it is actually a chunk before emitting.
                            let chunk_kv_key = chunk_key(&ctx.scope, ulid);
                            if let Ok(Some(_)) =
                                ctx.storage.read_as_of(&ctx.scope, &chunk_kv_key, snapshot).await
                            {
                                leaf_ids.push((id_bytes, score));
                            }
                        }
                        continue;
                    }
                };

                // Process Community.members.
                for member_id in &community.members {
                    let member_bytes = member_id.to_bytes().to_vec();
                    if !seen.insert(member_bytes.clone()) {
                        continue; // already visited
                    }

                    // Probe: is this member a sub-community or a leaf chunk?
                    let comm_key = community_key(&ctx.scope, *member_id);
                    let is_community = ctx
                        .storage
                        .read_as_of(&ctx.scope, &comm_key, snapshot)
                        .await
                        .ok()
                        .and_then(|r| r)
                        .and_then(|r| serde_json::from_slice::<Community>(&r.value).ok())
                        .is_some();

                    if is_community && depth + 1 < self.depth {
                        // Sub-community within descent budget — recurse.
                        queue.push_back((*member_id, score, depth + 1));
                    } else if !is_community {
                        // Leaf chunk: emit as a RawHit with the parent community's score.
                        leaf_ids.push((member_bytes, score));
                    }
                    // If is_community but depth budget exhausted: silently skip
                    // (the community summary is not a chunk, and we can't descend further).
                }
            }
        }

        // Dedup by id (a chunk can appear under multiple communities via
        // `members` cross-listing; keep the highest score).
        let mut by_id: std::collections::HashMap<Vec<u8>, f32> =
            std::collections::HashMap::with_capacity(leaf_ids.len());
        for (id, score) in leaf_ids {
            by_id.entry(id).and_modify(|s| *s = s.max(score)).or_insert(score);
        }

        Ok(by_id
            .into_iter()
            .map(|(id, score)| RawHit {
                id,
                score,
                rerank_applied: false,
                degraded: false,
                metadata: serde_json::Value::Null,
                source_op: SourceOp::Tree,
            })
            .collect())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Parse a ULID from its 16-byte big-endian representation.
#[inline]
fn ulid_from_bytes(bytes: &[u8]) -> Option<ulid::Ulid> {
    let arr: [u8; 16] = bytes.try_into().ok()?;
    Some(ulid::Ulid::from_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_new_records_index_and_clamps_k() {
        let t = Tree::new("communities", 5);
        assert_eq!(t.index, "communities");
        assert_eq!(t.k, 5);
        assert_eq!(t.depth, DEFAULT_TREE_DEPTH);
    }

    #[test]
    fn tree_k_is_clamped_to_max_k() {
        let t = Tree::new("communities", 1_000_000);
        assert_eq!(t.k, super::super::MAX_K);
    }

    #[test]
    fn tree_with_depth_clamps_to_max() {
        let t = Tree::new("communities", 5).with_depth(999);
        assert_eq!(t.depth, MAX_TREE_DEPTH);
    }

    #[test]
    fn tree_with_depth_enforces_min_1() {
        let t = Tree::new("communities", 5).with_depth(0);
        assert_eq!(t.depth, 1);
    }

    #[test]
    fn ulid_from_bytes_round_trips() {
        let id = ulid::Ulid::new();
        let bytes = id.to_bytes().to_vec();
        assert_eq!(ulid_from_bytes(&bytes), Some(id));
    }

    #[test]
    fn ulid_from_bytes_rejects_wrong_len() {
        assert_eq!(ulid_from_bytes(b"too-short"), None);
    }
}
