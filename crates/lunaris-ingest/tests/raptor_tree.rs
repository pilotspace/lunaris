//! Phase 29 Plan 01 — RAPTOR tree topology tests (Fixtures 1, 2 + determinism).
//!
//! RED phase: all tests fail because `raptor_community_id` and `build_raptor_tree`
//! do not yet exist.

use lunaris_core::{HlcClock, Scope};
use lunaris_ingest::{
    DocTree, TocNode, TocNodeId,
    build_raptor_tree, raptor_community_id,
};

/// Build a three-level DocTree (H1 > H2 > H3) for use in Fixture 1.
///
/// Structure:
///   H1 "Intro"  (char_span=(0, 20), level=1)
///     H2 "Background"  (char_span=(20, 60), level=2)
///       H3 "Details"  (char_span=(60, 100), level=3)
fn three_level_doctree(scope: &str, source: &str) -> DocTree {
    let h3_id = TocNodeId::from_fields(scope, source, 3, "Details");
    let h3 = TocNode {
        level: 3,
        title: "Details".into(),
        char_span: (60, 100),
        node_id: h3_id,
        children: vec![],
    };
    let h2_id = TocNodeId::from_fields(scope, source, 2, "Background");
    let h2 = TocNode {
        level: 2,
        title: "Background".into(),
        char_span: (20, 60),
        node_id: h2_id,
        children: vec![h3],
    };
    let h1_id = TocNodeId::from_fields(scope, source, 1, "Intro");
    let h1 = TocNode {
        level: 1,
        title: "Intro".into(),
        char_span: (0, 20),
        node_id: h1_id,
        children: vec![h2],
    };
    DocTree { roots: vec![h1] }
}

/// Build a no-headings DocTree (single synthetic root, level=0).
fn no_heading_doctree(scope: &str, source: &str, source_char_len: usize) -> DocTree {
    let root_id = TocNodeId::from_fields(scope, source, 0, "");
    let root = TocNode {
        level: 0,
        title: "".into(),
        char_span: (0, source_char_len),
        node_id: root_id,
        children: vec![],
    };
    DocTree { roots: vec![root] }
}

/// Construct a minimal Chunk with the given heading_path.
fn make_chunk(
    scope: &Scope,
    heading_path: Vec<String>,
    text: &str,
    clock: &HlcClock,
) -> lunaris_core::Chunk {
    use ulid::Ulid;
    lunaris_core::Chunk::new(
        scope.clone(),
        Ulid::new(), // episode_id stub
        text,
        1,
        0,
        heading_path,
        clock,
    )
}

// ---------------------------------------------------------------------------
// Fixture 1: multi-level heading document (H1 > H2 > H3)
// ---------------------------------------------------------------------------

/// Fixture 1 — multi-level headings produce 3 Communities with correct nesting.
#[tokio::test]
async fn fixture_1_multi_level_headings() {
    let scope = Scope::dev();
    let source = "doc.md";
    let clock = HlcClock::new(0);

    let doctree = three_level_doctree(scope.as_str(), source);

    // One leaf under H3, one leaf under H2, one leaf under H1.
    let leaf_h3 = make_chunk(&scope, vec!["Intro".into(), "Background".into(), "Details".into()], "Details text", &clock);
    let leaf_h2 = make_chunk(&scope, vec!["Intro".into(), "Background".into()], "Background text", &clock);
    let leaf_h1 = make_chunk(&scope, vec!["Intro".into()], "Intro text", &clock);
    let chunks = vec![leaf_h1.clone(), leaf_h2.clone(), leaf_h3.clone()];

    let (communities, wired) = build_raptor_tree(&doctree, &chunks, &scope, source, &clock);

    // a. Exactly 3 Communities (one per heading section).
    assert_eq!(communities.len(), 3, "expected 3 communities; got {}", communities.len());

    // b-d. Find communities by level.
    let c1 = communities.iter().find(|c| c.level == 1).expect("H1 community missing");
    let c2 = communities.iter().find(|c| c.level == 2).expect("H2 community missing");
    let c3 = communities.iter().find(|c| c.level == 3).expect("H3 community missing");

    // H3.parent == H2.id
    assert_eq!(c3.parent, Some(c2.id), "H3 community parent must be H2");
    // H2.parent == H1.id
    assert_eq!(c2.parent, Some(c1.id), "H2 community parent must be H1");
    // H1.parent == None
    assert_eq!(c1.parent, None, "H1 community must have no parent");

    // e. Leaf parent_ids.
    let leaf_h3_wired = wired.iter().find(|c| c.id == leaf_h3.id).expect("leaf_h3 missing");
    let leaf_h2_wired = wired.iter().find(|c| c.id == leaf_h2.id).expect("leaf_h2 missing");
    let leaf_h1_wired = wired.iter().find(|c| c.id == leaf_h1.id).expect("leaf_h1 missing");

    assert_eq!(leaf_h3_wired.parent_id, Some(c3.id), "H3 leaf must point to H3 community");
    assert_eq!(leaf_h2_wired.parent_id, Some(c2.id), "H2 leaf must point to H2 community");
    assert_eq!(leaf_h1_wired.parent_id, Some(c1.id), "H1 leaf must point to H1 community");

    // f. Members contain their direct-child community IDs + leaf IDs.
    assert!(c3.members.contains(&leaf_h3.id), "H3 community must include H3 leaf");
    assert!(c2.members.contains(&leaf_h2.id), "H2 community must include H2 leaf");
    assert!(c2.members.contains(&c3.id), "H2 community must include H3 sub-community ID");
    assert!(c1.members.contains(&leaf_h1.id), "H1 community must include H1 leaf");
    assert!(c1.members.contains(&c2.id), "H1 community must include H2 sub-community ID");

    // g. Level values.
    assert_eq!(c1.level, 1);
    assert_eq!(c2.level, 2);
    assert_eq!(c3.level, 3);

    // D4 guardrail: summary_embedding must remain None.
    for c in &communities {
        assert!(c.summary_embedding.is_none(), "summary_embedding must be None (D4)");
    }
}

// ---------------------------------------------------------------------------
// Fixture 2: no-headings document → single root Community
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_2_no_headings() {
    let scope = Scope::dev();
    let source = "flat.md";
    let clock = HlcClock::new(0);
    let source_char_len = 100usize;

    let doctree = no_heading_doctree(scope.as_str(), source, source_char_len);

    // Three leaves with empty heading_path (no headings).
    let leaf1 = make_chunk(&scope, vec![], "text one", &clock);
    let leaf2 = make_chunk(&scope, vec![], "text two", &clock);
    let leaf3 = make_chunk(&scope, vec![], "text three", &clock);
    let chunks = vec![leaf1.clone(), leaf2.clone(), leaf3.clone()];

    let (communities, wired) = build_raptor_tree(&doctree, &chunks, &scope, source, &clock);

    // a. Exactly 1 Community (the root).
    assert_eq!(communities.len(), 1, "no-headings must produce exactly 1 community");
    let root = &communities[0];

    // b. Root has no parent and level == 0.
    assert_eq!(root.parent, None);
    assert_eq!(root.level, 0);

    // c. All three leaves point to the root.
    for lf in wired.iter() {
        assert_eq!(lf.parent_id, Some(root.id), "all leaves must point to root community");
    }

    // d. Root members contains all three leaf IDs.
    assert!(root.members.contains(&leaf1.id));
    assert!(root.members.contains(&leaf2.id));
    assert!(root.members.contains(&leaf3.id));

    // D4 guardrail.
    assert!(root.summary_embedding.is_none(), "summary_embedding must be None (D4)");
}

// ---------------------------------------------------------------------------
// Determinism: same inputs → same community ID
// ---------------------------------------------------------------------------

#[test]
fn raptor_community_id_is_deterministic() {
    let scope = "my-scope";
    let source = "doc.md";
    let char_span = (10usize, 50usize);
    let level = 2u8;

    let id_a = raptor_community_id(scope, source, char_span, level);
    let id_b = raptor_community_id(scope, source, char_span, level);
    assert_eq!(id_a, id_b, "raptor_community_id must be deterministic");
}

// ---------------------------------------------------------------------------
// Duplicate-title safety: same title, different char_span → different IDs
// ---------------------------------------------------------------------------

#[test]
fn raptor_community_id_differs_for_different_char_spans() {
    let scope = "my-scope";
    let source = "doc.md";
    let level = 1u8;

    let id_a = raptor_community_id(scope, source, (0, 50), level);
    let id_b = raptor_community_id(scope, source, (51, 100), level);
    assert_ne!(id_a, id_b, "different char_spans must produce different community IDs");
}
