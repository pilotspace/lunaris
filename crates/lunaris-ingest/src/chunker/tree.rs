//! DocTree + TocNode — STRUCT-02.
//!
//! A [`DocTree`] captures the heading hierarchy of a markdown document as a
//! navigable tree of [`TocNode`]s. One DocTree is produced per ingest Episode
//! and persisted as a single `WriteOp::KvPut` inside the same atomic write
//! as the chunks (INGEST-04 preserved).
//!
//! ## No-heading documents
//!
//! A document with NO headings yields a single-root DocTree (one [`TocNode`]
//! with `level = 0`, `title = ""`, spanning the entire document). This
//! guarantees the tree is never empty.
//!
//! ## Deterministic node_id
//!
//! Each node's `node_id` is a 16-byte blake3 truncation of:
//! `blake3(scope || "::" || source || "::" || level_str || "::" || title)`
//! mirroring the `CommunityId::from_members` / `EntityId` pattern from
//! `lunaris-consolidate`.
//!
//! ## char_span
//!
//! `TocNode.char_span` holds the `(start, end)` character offsets of the
//! heading's byte range in the original source, converted from
//! `pulldown_cmark::into_offset_iter()` byte offsets via
//! `source[..byte_pos].chars().count()`. ASCII documents: char == byte.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// 16-byte deterministic blake3-derived node identifier.
///
/// Derived from `blake3(scope || "::" || source || "::" || level_str || "::" || title)[..16]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TocNodeId(pub [u8; 16]);

impl TocNodeId {
    /// Derive a deterministic id from the node's identifying fields.
    pub fn from_fields(scope: &str, source: &str, level: u8, title: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(scope.as_bytes());
        hasher.update(b"::");
        hasher.update(source.as_bytes());
        hasher.update(b"::");
        hasher.update(level.to_string().as_bytes());
        hasher.update(b"::");
        hasher.update(title.as_bytes());
        let h = hasher.finalize();
        let bytes: [u8; 16] =
            h.as_bytes()[..16].try_into().expect("blake3 output is 32 bytes; first 16 always fit");
        TocNodeId(bytes)
    }
}

/// A single node in the document's table-of-contents tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TocNode {
    /// Heading level (1–6). `0` for the synthetic root of a no-heading document.
    pub level: u8,
    /// Heading text (empty for the synthetic root node).
    pub title: String,
    /// `(start_char, end_char)` character offsets in the original source.
    /// Computed from `pulldown_cmark::into_offset_iter()` byte offsets by
    /// counting `source[..byte_pos].chars().count()`. Not char indices into
    /// `title` — these are positions in the *original markdown source*.
    pub char_span: (usize, usize),
    /// Deterministic 16-byte blake3 node identifier.
    pub node_id: TocNodeId,
    /// Direct children of this node (headings at a deeper level).
    pub children: Vec<TocNode>,
}

impl TocNode {
    /// Construct a new [`TocNode`].
    pub fn new(level: u8, title: String, char_span: (usize, usize), node_id: TocNodeId) -> Self {
        Self { level, title, char_span, node_id, children: Vec::new() }
    }
}

/// The full document tree, rooted at a list of top-level [`TocNode`]s.
///
/// A document with no headings has exactly ONE root node (level=0) with an
/// empty title spanning `(0, source.chars().count())`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocTree {
    pub roots: Vec<TocNode>,
}

impl DocTree {
    /// An empty DocTree shell (use [`build_doctree`] to populate from source).
    pub fn empty() -> Self {
        Self { roots: Vec::new() }
    }
}

// ---------------------------------------------------------------------------
// Heading record — used by chunker to capture heading spans
// ---------------------------------------------------------------------------

/// Raw heading data captured during the `into_offset_iter()` parse pass.
#[derive(Clone, Debug)]
pub struct HeadingRecord {
    pub level: u8,
    pub title: String,
    /// Character offsets `(start, end)` in the original source.
    pub char_span: (usize, usize),
}

// ---------------------------------------------------------------------------
// Tree builder
// ---------------------------------------------------------------------------

/// Build a [`DocTree`] from a list of [`HeadingRecord`]s extracted during the
/// chunker parse pass.
///
/// If `records` is empty, returns a single-root tree with a synthetic root
/// node (level=0, empty title, span=(0, source_char_len)).
pub fn build_doctree(
    records: &[HeadingRecord],
    scope: &str,
    source: &str,
    source_char_len: usize,
) -> DocTree {
    if records.is_empty() {
        let node_id = TocNodeId::from_fields(scope, source, 0, "");
        let root = TocNode::new(0, String::new(), (0, source_char_len), node_id);
        return DocTree { roots: vec![root] };
    }

    // Build the tree with a stack-based algorithm mirroring the chunker's
    // heading_stack approach. Stack entries: (level, index into working_nodes).
    // `working_nodes` is a flat Vec; we track parent indices explicitly.
    let mut roots: Vec<TocNode> = Vec::new();
    // Stack: Vec of (level, mutable path to the node).
    // We build the tree recursively via a stack of "current lineage" nodes.
    // Use a simpler approach: maintain a stack of (level, node) and fold into
    // the tree structure once complete.

    // Build flat nodes first, then wire parents.
    let flat: Vec<TocNode> = records
        .iter()
        .map(|r| {
            let node_id = TocNodeId::from_fields(scope, source, r.level, &r.title);
            TocNode::new(r.level, r.title.clone(), r.char_span, node_id)
        })
        .collect();

    // Insert nodes into tree respecting level hierarchy.
    // Use a mutable stack of (level, index) where index references `roots` or children.
    // Simplest approach: build iteratively into a flat Vec<TocNode> with ownership.
    insert_into_tree(flat, &mut roots);

    DocTree { roots }
}

/// Recursively insert `flat` nodes into `roots` using heading level to determine nesting.
fn insert_into_tree(flat: Vec<TocNode>, roots: &mut Vec<TocNode>) {
    // Stack holds the "path" of ancestor nodes whose children list is currently open.
    // Each entry is a mutable reference path — but we can't do that with a simple Vec.
    // Use indices into a growing structure instead.

    // We build a Vec<(node, parent_stack_depth)> then assemble.
    // Simpler: process iteratively, maintaining a mutable stack by rebuilding.

    for node in flat {
        let level = node.level;
        insert_node(roots, node, level);
    }
}

/// Insert `node` into the tree rooted at `roots` based on heading level semantics.
fn insert_node(roots: &mut Vec<TocNode>, node: TocNode, _level: u8) {
    // Find the deepest open parent that has a strictly lower level.
    // If none found, push to roots.
    if roots.is_empty() {
        roots.push(node);
        return;
    }
    // Try to insert as child of the last root (recursively).
    if roots.last_mut().is_some_and(|last| node.level > last.level) {
        insert_node(&mut roots.last_mut().unwrap().children, node, _level);
        return;
    }
    // Level is <= last root level: push as new root sibling.
    roots.push(node);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(level: u8, title: &str, start: usize, end: usize) -> HeadingRecord {
        HeadingRecord { level, title: title.to_string(), char_span: (start, end) }
    }

    #[test]
    fn no_heading_doc_yields_single_root_tree() {
        let tree = build_doctree(&[], "scope", "source.md", 100);
        assert_eq!(tree.roots.len(), 1, "must have exactly one root");
        assert_eq!(tree.roots[0].level, 0, "synthetic root must have level 0");
        assert_eq!(tree.roots[0].title, "", "synthetic root must have empty title");
        assert_eq!(tree.roots[0].char_span, (0, 100));
        assert!(tree.roots[0].children.is_empty());
    }

    #[test]
    fn mixed_heading_doc_yields_correct_hierarchy() {
        let records = vec![
            make_record(1, "Introduction", 0, 12),
            make_record(2, "Background", 13, 23),
            make_record(2, "Methods", 24, 31),
            make_record(1, "Conclusion", 32, 42),
        ];
        let tree = build_doctree(&records, "scope", "doc.md", 100);
        assert_eq!(tree.roots.len(), 2, "two H1 roots; got {:?}", tree.roots.len());
        assert_eq!(tree.roots[0].title, "Introduction");
        assert_eq!(tree.roots[0].children.len(), 2, "Introduction must have 2 H2 children");
        assert_eq!(tree.roots[0].children[0].title, "Background");
        assert_eq!(tree.roots[0].children[1].title, "Methods");
        assert_eq!(tree.roots[1].title, "Conclusion");
    }

    #[test]
    fn node_id_is_deterministic() {
        let id1 = TocNodeId::from_fields("scope", "doc.md", 1, "Introduction");
        let id2 = TocNodeId::from_fields("scope", "doc.md", 1, "Introduction");
        assert_eq!(id1, id2, "same inputs must produce same node_id");

        let id3 = TocNodeId::from_fields("other-scope", "doc.md", 1, "Introduction");
        assert_ne!(id1, id3, "different scope must produce different node_id");
    }

    #[test]
    fn doctree_roundtrips_serde() {
        let records = vec![make_record(1, "Title", 0, 5)];
        let tree = build_doctree(&records, "s", "f.md", 50);
        let json = serde_json::to_string(&tree).expect("serialize");
        let back: DocTree = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(tree, back);
    }

    #[test]
    fn doctree_key_format() {
        use lunaris_core::{Scope, keyspace::doctree_key};
        let scope = Scope::new("test-scope").unwrap();
        let id = ulid::Ulid::nil();
        let key = doctree_key(&scope, id);
        let key_str = std::str::from_utf8(&key).unwrap();
        assert!(
            key_str.starts_with("lunaris:test-scope:doctree:"),
            "doctree key must start with correct prefix; got: {key_str}"
        );
    }
}
