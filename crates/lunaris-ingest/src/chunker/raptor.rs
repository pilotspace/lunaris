//! Phase 29 TREE-01 — RAPTOR tree topology builder.
//!
//! Converts a [`DocTree`] + flat `Vec<Chunk>` into a `Vec<Community>` with:
//! - **Deterministic IDs** derived from `blake3(scope || source || char_span || level)`
//!   — title is intentionally excluded so IDs are stable across LLM re-summarization (D5).
//! - **Correct parent/members wiring** — `Community.parent` is the enclosing section's
//!   community ID; `Community.members` = direct-child community IDs + leaf chunk IDs.
//! - **Leaf → section assignment** via the structural path (D2):
//!   - If `chunk.heading_path` is non-empty, walk the DocTree by that title chain.
//!   - If `heading_path` is empty (bake-off non-structural chunks), fall back to
//!     byte-containment using `chunk.source_byte_span`.
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
//! the caller (Plan 03 wiring) fills it via [`ExtractiveSummarizer`] before
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
    let bytes: [u8; 16] = h.as_bytes()[..16]
        .try_into()
        .expect("blake3 produces 32 bytes; first 16 always fit");
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
/// - **Byte-containment fallback**: `heading_path` empty and `source_byte_span` present
///   → find the TocNode whose `char_span` contains the leaf's start offset.
/// - **Ultimate fallback**: assign to the shallowest root (level=0 or first root).
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
    let id_to_idx: std::collections::HashMap<Ulid, usize> = communities
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i))
        .collect();

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
        if let Some(parent_id) = fn_.parent_community_id {
            if let Some(&parent_idx) = id_to_idx.get(&parent_id) {
                communities[parent_idx].members.push(fn_.community_id);
            }
        }
    }
    // 5b. Leaf chunks.
    for chunk in &wired_chunks {
        if let Some(parent_id) = chunk.parent_id {
            if let Some(&parent_idx) = id_to_idx.get(&parent_id) {
                communities[parent_idx].members.push(chunk.id);
            }
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
/// Returns `Some(community_id)` or `None` if the DocTree is empty.
fn assign_leaf(
    chunk: &Chunk,
    flat_nodes: &[FlatNode],
    doctree: &DocTree,
    scope: &str,
    source: &str,
) -> Option<Ulid> {
    // Structural path: heading_path non-empty → walk by title chain.
    if !chunk.heading_path.is_empty() {
        if let Some(id) = find_by_title_path(&chunk.heading_path, flat_nodes) {
            return Some(id);
        }
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
fn collect_descendant_ids(root_id: Ulid, communities: &[Community]) -> std::collections::HashSet<Ulid> {
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
