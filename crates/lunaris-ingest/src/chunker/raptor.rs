//! Phase 29 TREE-01 — RAPTOR tree topology builder.
//!
//! Converts a [`DocTree`] + flat `Vec<Chunk>` into a `Vec<Community>` with:
//! - **Deterministic IDs** derived from `blake3(scope || source || char_span || level)`
//!   — title is intentionally excluded so IDs are stable across LLM re-summarization (D5).
//! - **Correct parent/members wiring** — `Community.parent` is the enclosing section's
//!   community ID; `Community.members` = direct-child community IDs + leaf chunk IDs.
//! - **Leaf → section assignment** via the structural path (D2):
//!   - If `chunk.heading_path` is non-empty, walk the DocTree by that title chain.
//!
//! ## Byte-containment — NOT IMPLEMENTED (Phase 29 known limitation)
//!
//! D2 specifies a byte-containment fallback for non-structural bake-off winners
//! (chunks with empty `heading_path`). That path is **not wired** in Phase 29:
//! `source_byte_span` lives on `ChunkDraft` and is dropped after `into_chunk()`,
//! so it is unavailable here.
//!
//! **Current behaviour for empty-`heading_path` chunks** (e.g. `SemanticBreakpointGenerator`
//! winners from `ingest_episode_with_bakeoff`): they fall through to the ultimate fallback
//! and are assigned to `doctree.roots[0]`, regardless of the actual document section they
//! came from. This collapses multi-level tree topology for non-structural winners.
//!
//! Deferral rationale: propagating `source_byte_span` onto `Chunk` requires a core primitive
//! schema change plus a units reconciliation (`source_byte_span` is bytes; `TocNode.char_span`
//! is chars). Deferred to a future phase. See `TODO(phase-future)` in `assign_leaf`.
//!
//! ## INGEST-04 compliance
//!
//! This module only **produces** `Community` values and mutated `Chunk`s — it does NOT
//! call `storage.atomic_write`. The single write call remains in `pipeline.rs`.
//!
//! ## D4 guardrail
//!
//! `Community.summary_embedding` is NEVER set in this module.
//! Summary text is initialised to `String::new()` as a placeholder;
//! the caller (Plan 03 wiring) fills it via `ExtractiveSummarizer` before
//! the `atomic_write` call.
//!
//! ## Line-count budget
//! This module MUST stay under 1 000 lines (per CLAUDE.md §File size).

use blake3::Hasher;
use lunaris_core::{Chunk, HlcClock, Scope, primitives::Community};
use ulid::Ulid;

use super::tree::{DocTree, TocNode};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Derive the deterministic community ID for a RAPTOR internal node.
///
/// Preimage: `scope || "::" || source || "::" || char_span.0 || "::" || char_span.1 || "::" || level`.
/// Title is **excluded** (D5 locked decision) so that LLM re-summarisation does
/// not cause node ID churn on re-ingest.
pub fn raptor_community_id(
    scope: &str,
    source: &str,
    char_span: (usize, usize),
    level: u8,
) -> Ulid {
    let mut hasher = Hasher::new();
    hasher.update(scope.as_bytes());
    hasher.update(b"::");
    hasher.update(source.as_bytes());
    hasher.update(b"::");
    hasher.update(char_span.0.to_string().as_bytes());
    hasher.update(b"::");
    hasher.update(char_span.1.to_string().as_bytes());
    hasher.update(b"::");
    hasher.update(level.to_string().as_bytes());
    let h = hasher.finalize();
    let bytes: [u8; 16] =
        h.as_bytes()[..16].try_into().expect("blake3 produces 32 bytes; first 16 always fit");
    Ulid::from_bytes(bytes)
}

/// Build the RAPTOR community tree from a [`DocTree`] and a flat list of [`Chunk`]s.
///
/// # Returns
/// `(communities, wired_chunks)` where:
/// - `communities` is the flat `Vec<Community>` covering every heading section.
///   Each entry has `id`, `level`, `parent`, and `members` set.
///   `summary` is an empty placeholder (filled by [`super::super::summarizer`]).
///   `summary_embedding` is `None` — D4 guardrail, never set here.
/// - `wired_chunks` is a clone of the input chunks with `parent_id` set to the
///   ID of the nearest enclosing section Community.
///
/// # Leaf assignment (D2)
/// - **Structural path** (default): `chunk.heading_path` non-empty → walk the DocTree
///   by title chain to find the deepest matching TocNode.
/// - **Ultimate fallback**: `heading_path` empty → assign to `doctree.roots[0]`.
///
/// Note: byte-containment for non-structural bake-off winners is **not implemented**
/// in Phase 29. See `assign_leaf` and the module-level doc for the deferral rationale.
pub fn build_raptor_tree(
    doctree: &DocTree,
    chunks: &[Chunk],
    scope: &Scope,
    source: &str,
    clock: &HlcClock,
) -> (Vec<Community>, Vec<Chunk>) {
    // Step 1: flatten the DocTree into an ordered list of TocNodes (post-order
    // so children come before parents, matching bottom-up summarisation in Plan 03).
    let mut flat_nodes: Vec<FlatNode> = Vec::new();
    for root in &doctree.roots {
        collect_nodes(root, None, scope.as_str(), source, &mut flat_nodes);
    }

    // Step 2: build Community stubs (no summary, no members yet).
    let mut communities: Vec<Community> = flat_nodes
        .iter()
        .map(|fn_| {
            let mut c = Community::new(scope.clone(), fn_.level, "", clock);
            // Override the random id from Community::new with the deterministic one.
            c.id = fn_.community_id;
            c.parent = fn_.parent_community_id;
            // D4: summary_embedding stays None — never set in this module.
            debug_assert!(c.summary_embedding.is_none(), "D4: summary_embedding must be None");
            c
        })
        .collect();

    // Step 3: build a lookup from community_id → position in `communities`.
    let id_to_idx: std::collections::HashMap<Ulid, usize> =
        communities.iter().enumerate().map(|(i, c)| (c.id, i)).collect();

    // Step 4: assign each leaf chunk to its enclosing community.
    let mut wired_chunks: Vec<Chunk> = chunks.to_vec();
    for chunk in wired_chunks.iter_mut() {
        let parent_community_id = assign_leaf(chunk, &flat_nodes, doctree, scope.as_str(), source);
        chunk.parent_id = parent_community_id;
    }

    // Step 5: populate Community.members.
    // For each community: collect direct-child community IDs (communities whose parent == this id)
    // + leaf chunk IDs whose parent_id == this community's id.

    // 5a. Direct sub-community children.
    for fn_ in &flat_nodes {
        if let Some(parent_idx) = fn_.parent_community_id.and_then(|id| id_to_idx.get(&id).copied())
        {
            communities[parent_idx].members.push(fn_.community_id);
        }
    }
    // 5b. Leaf chunks.
    for chunk in &wired_chunks {
        if let Some(parent_idx) = chunk.parent_id.and_then(|id| id_to_idx.get(&id).copied()) {
            communities[parent_idx].members.push(chunk.id);
        }
    }

    (communities, wired_chunks)
}

/// Returns the texts of all leaf chunks that are descendants of `root_id`
/// (inclusive of direct children of `root_id`).
///
/// Used by `pipeline.rs` to build `SummaryInput.child_texts` for the bottom-up
/// summarisation pass. A "leaf" is any chunk whose `parent_id` falls within the
/// community's transitively-reachable descendant set.
pub fn chunk_texts_for_community(
    root_id: Ulid,
    communities: &[Community],
    chunks: &[Chunk],
) -> Vec<String> {
    // Collect all descendant community IDs (including root_id itself) via DFS.
    let descendant_ids = collect_descendant_ids(root_id, communities);

    // Collect chunk texts whose parent_id is in that set.
    chunks
        .iter()
        .filter(|c| c.parent_id.map(|pid| descendant_ids.contains(&pid)).unwrap_or(false))
        .map(|c| c.text.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// A flattened representation of a TocNode with its pre-computed community ID.
struct FlatNode {
    level: u8,
    // char_span and title retained for the byte-containment fallback path
    // (currently unused because Chunk loses source_byte_span after into_chunk;
    // see TODO in assign_leaf). Suppressed to avoid noise.
    #[allow(dead_code)]
    char_span: (usize, usize),
    #[allow(dead_code)]
    title: String,
    community_id: Ulid,
    parent_community_id: Option<Ulid>,
    /// Full title path from root to this node (for structural leaf assignment).
    title_path: Vec<String>,
}

/// Recursively collect TocNodes in pre-order (root before children).
/// Pre-order means that when we later assign leaf chunks by title-path walk,
/// we process the hierarchy top-down.
fn collect_nodes(
    node: &TocNode,
    parent_community_id: Option<Ulid>,
    scope: &str,
    source: &str,
    out: &mut Vec<FlatNode>,
) {
    let community_id = raptor_community_id(scope, source, node.char_span, node.level);

    // Build the title path up to this node (prefix from parent + this title).
    let parent_title_path: Vec<String> = out
        .iter()
        .rev()
        .find(|fn_| fn_.community_id == parent_community_id.unwrap_or(Ulid::from_bytes([0; 16])))
        .map(|fn_| fn_.title_path.clone())
        .unwrap_or_default();

    let mut title_path = parent_title_path;
    if !node.title.is_empty() {
        title_path.push(node.title.clone());
    }

    out.push(FlatNode {
        level: node.level,
        char_span: node.char_span,
        title: node.title.clone(),
        community_id,
        parent_community_id,
        title_path,
    });

    for child in &node.children {
        collect_nodes(child, Some(community_id), scope, source, out);
    }
}

/// Assign a leaf chunk to its nearest enclosing community.
///
/// # Assignment paths
///
/// 1. **Structural path** (default): `chunk.heading_path` non-empty → walk the DocTree
///    by that title chain to the deepest matching [`TocNode`].
/// 2. **Ultimate fallback**: `heading_path` empty → assign to `doctree.roots[0]`.
///
/// # Known limitation — byte-containment NOT implemented
///
/// D2 specifies a byte-containment path for non-structural bake-off winners
/// (`heading_path` empty, `source_byte_span` present). This path is **not wired**:
/// `source_byte_span` is dropped after `ChunkDraft::into_chunk()` and is unavailable
/// on [`Chunk`] at this call site. Non-structural winners therefore collapse to
/// `roots[0]`, destroying multi-level topology. See the module-level doc and
/// `TODO(phase-future)` below for the deferral rationale.
///
/// Returns `Some(community_id)` or `None` if the DocTree is empty.
fn assign_leaf(
    chunk: &Chunk,
    flat_nodes: &[FlatNode],
    doctree: &DocTree,
    scope: &str,
    source: &str,
) -> Option<Ulid> {
    // Structural path: heading_path non-empty → walk by title chain.
    if let Some(id) = (!chunk.heading_path.is_empty())
        .then(|| find_by_title_path(&chunk.heading_path, flat_nodes))
        .flatten()
    {
        return Some(id);
    }

    // Byte-containment fallback: source_byte_span is on ChunkDraft, not Chunk.
    // At this stage we only have a Chunk (source_byte_span is lost after into_chunk()).
    // The structural path (heading_path) is the primary path; no byte-containment fallback
    // available here. TODO(phase-future): propagate source_byte_span onto Chunk if needed.

    // Ultimate fallback: the shallowest / first node (synthetic root for no-heading docs,
    // or the first H1 for headed docs).
    if doctree.roots.is_empty() {
        return None;
    }
    let root = &doctree.roots[0];
    Some(raptor_community_id(scope, source, root.char_span, root.level))
}

/// Walk `flat_nodes` by the title path from `heading_path` and return the
/// community ID of the deepest matching node.
///
/// The algorithm looks for the node whose `title_path` equals the full
/// `heading_path` (deepest-match first, then progressively shorter prefixes).
/// This mirrors how `chunk.heading_path` was built: each entry is a title
/// from the heading stack at chunk-emit time.
fn find_by_title_path(heading_path: &[String], flat_nodes: &[FlatNode]) -> Option<Ulid> {
    // Try longest-match first (most specific section), then fall back to shorter prefixes.
    for len in (1..=heading_path.len()).rev() {
        let prefix = &heading_path[..len];
        if let Some(node) = flat_nodes.iter().find(|fn_| fn_.title_path == prefix) {
            return Some(node.community_id);
        }
    }
    None
}

/// Find the deepest TocNode whose `char_span` contains `byte_offset` as its start.
/// Retained for the opt-in byte-containment path (D2). Currently unused because
/// `source_byte_span` is on `ChunkDraft`, not `Chunk`; see TODO in `assign_leaf`.
#[allow(dead_code)]
fn find_by_byte_containment(byte_offset: usize, flat_nodes: &[FlatNode]) -> Option<Ulid> {
    // Among all nodes that contain the offset, pick the deepest level (most specific).
    flat_nodes
        .iter()
        .filter(|fn_| fn_.char_span.0 <= byte_offset && byte_offset < fn_.char_span.1)
        .max_by_key(|fn_| fn_.level)
        .map(|fn_| fn_.community_id)
}

/// Collect the set of all community IDs that are descendants of `root_id`
/// (including `root_id` itself).
#[cfg_attr(not(test), allow(dead_code))]
fn collect_descendant_ids(
    root_id: Ulid,
    communities: &[Community],
) -> std::collections::HashSet<Ulid> {
    let mut result = std::collections::HashSet::new();
    let mut stack = vec![root_id];
    while let Some(current) = stack.pop() {
        result.insert(current);
        // Find all communities whose parent == current.
        for c in communities {
            if c.parent == Some(current) {
                stack.push(c.id);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::{HlcClock, Scope};

    use crate::chunker::tree::{DocTree, TocNode, TocNodeId};

    const SCOPE: &str = "_dev_";
    const SOURCE: &str = "test.md";

    fn test_clock() -> std::sync::Arc<HlcClock> {
        HlcClock::new(0)
    }

    fn dev_scope() -> Scope {
        Scope::dev()
    }

    /// Build a minimal chunk with the given heading_path (no embedding, parent_id unset).
    fn make_chunk(
        scope: Scope,
        episode_id: ulid::Ulid,
        heading_path: Vec<String>,
    ) -> lunaris_core::Chunk {
        let clock = test_clock();
        let mut c =
            lunaris_core::Chunk::new(scope, episode_id, "dummy text", 2, 0, heading_path, &clock);
        c.embedding = Some(vec![0.0f32; 4]);
        c
    }

    /// Build a DocTree with two H1 roots, the second having one H2 child.
    ///
    /// ```text
    /// H1 "Overview"      (roots[0])
    /// H1 "Details"       (roots[1])
    ///   H2 "Sub-details" (child of roots[1])
    /// ```
    fn two_root_doctree() -> DocTree {
        let h1_overview = TocNode::new(
            1,
            "Overview".to_string(),
            (0, 100),
            TocNodeId::from_fields(SCOPE, SOURCE, 1, "Overview"),
        );
        let h2_sub = TocNode::new(
            2,
            "Sub-details".to_string(),
            (150, 250),
            TocNodeId::from_fields(SCOPE, SOURCE, 2, "Sub-details"),
        );
        let mut h1_details = TocNode::new(
            1,
            "Details".to_string(),
            (100, 300),
            TocNodeId::from_fields(SCOPE, SOURCE, 1, "Details"),
        );
        h1_details.children.push(h2_sub);
        DocTree { roots: vec![h1_overview, h1_details] }
    }

    /// Discriminating test for the known Phase 29 limitation (Warning 1):
    ///
    /// A chunk with empty `heading_path` (e.g. produced by `SemanticBreakpointGenerator`
    /// winning the bake-off) is assigned to `doctree.roots[0]`, NOT to the section
    /// whose byte span contains the chunk. This is the documented "ultimate fallback"
    /// behavior — byte-containment is NOT implemented in Phase 29.
    ///
    /// This test encodes the known-limited behavior so any future change (e.g. wiring
    /// byte-containment) will require explicitly updating this assertion.
    #[test]
    fn empty_heading_path_chunk_lands_on_roots_zero() {
        let scope = dev_scope();
        let episode_id = ulid::Ulid::new();
        let clock = test_clock();
        let doctree = two_root_doctree();

        // Chunk with NO heading_path — simulates a SemanticBreakpointGenerator winner.
        let chunk = make_chunk(scope.clone(), episode_id, vec![]);

        let (_, wired_chunks) = build_raptor_tree(&doctree, &[chunk], &scope, SOURCE, &clock);

        assert_eq!(wired_chunks.len(), 1);
        let wired = &wired_chunks[0];

        // The chunk MUST land on roots[0] (the "Overview" H1), NOT on roots[1]
        // or any of its children — byte-containment is not implemented.
        let root0 = &doctree.roots[0];
        let expected_parent = raptor_community_id(SCOPE, SOURCE, root0.char_span, root0.level);

        assert_eq!(
            wired.parent_id,
            Some(expected_parent),
            "Phase 29 known limitation: empty heading_path chunk must land on roots[0] \
             (byte-containment NOT implemented). \
             If this fails, byte-containment has been wired — update this test."
        );
    }

    /// Structural path: a chunk with a populated heading_path lands on the matching section.
    #[test]
    fn structural_heading_path_chunk_lands_on_correct_section() {
        let scope = dev_scope();
        let episode_id = ulid::Ulid::new();
        let clock = test_clock();
        let doctree = two_root_doctree();

        // Chunk whose heading_path walks to "Details" > "Sub-details".
        let chunk = make_chunk(
            scope.clone(),
            episode_id,
            vec!["Details".to_string(), "Sub-details".to_string()],
        );

        let (_, wired_chunks) = build_raptor_tree(&doctree, &[chunk], &scope, SOURCE, &clock);

        assert_eq!(wired_chunks.len(), 1);
        let wired = &wired_chunks[0];

        // Must land on H2 "Sub-details", not on roots[0] or roots[1].
        let h2_sub = &doctree.roots[1].children[0];
        let expected_parent = raptor_community_id(SCOPE, SOURCE, h2_sub.char_span, h2_sub.level);

        assert_eq!(
            wired.parent_id,
            Some(expected_parent),
            "structural path must place the chunk in the deepest matching TocNode"
        );
    }
}
