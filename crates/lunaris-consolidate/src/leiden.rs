//! Plan 04-02 Task 2 — community detection per D-15 Option A: hand-rolled
//! label-propagation.
//!
//! ## Why label-propagation, not real Leiden
//!
//! Per D-15 the v0 consolidator uses "Leiden-equivalent default" =
//! label-propagation. The two real Rust implementations of Leiden
//! (`rustworkx`, `petgraph`-based) both carry unsafe blocks in their
//! transitive closure and would violate `#![forbid(unsafe_code)]` declared at
//! the crate root. Phase 5 may swap to a real Leiden impl when a no-unsafe
//! crate ships; until then label-propagation keeps the trait surface
//! satisfied + the build clean.
//!
//! Reference: Raghavan et al. (2007) "Near linear time algorithm to detect
//! community structures in large-scale networks." Phys. Rev. E 76: 036106.
//!
//! ## Algorithm
//!
//! 1. Initialize each node in its own community (singleton labels).
//! 2. Repeat up to `max_iters`:
//!    - Visit nodes in fixed order (by `EntityId.0` byte order — deterministic).
//!    - For each node, set its label to the most-frequent label among
//!      its neighbors (ties broken by smallest `EntityId.0`).
//!    - If no node changed labels this iteration, stop.
//! 3. Materialize the final labels as `CommunityId::from_members(sorted_set)`
//!    so the output `CommunityId` is content-hashed by member set.
//!
//! Determinism is critical for the `assert_eq!` shape in unit tests: the
//! visit order, tie-break, and `CommunityId` derivation are all fixed
//! per-input — same `GraphSnapshot` always produces the same
//! `CommunityAssignment`.

use std::collections::HashMap;

use lunaris_extract::EntityId;

use crate::types::CommunityId;

/// A minimal graph snapshot used by [`leiden_pass`]. Plan 04-04 wires the
/// umbrella handle to build one of these from the `StoragePort::graph_traverse`
/// result; Plan 04-02 keeps the shape pure-data so unit tests can build tiny
/// graphs without a live backend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSnapshot {
    /// Nodes in the graph. Duplicates are tolerated but ignored on second
    /// occurrence (the first occurrence wins).
    pub nodes: Vec<EntityId>,
    /// Undirected edges (a, b). The label-propagation pass treats each edge
    /// as bidirectional; self-loops are allowed but contribute nothing to
    /// community formation.
    pub edges: Vec<(EntityId, EntityId)>,
}

/// Output of one label-propagation pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommunityAssignment {
    /// Map from `EntityId` → `CommunityId`. Every node in the input graph
    /// appears exactly once; isolated nodes form singleton communities.
    pub by_node: HashMap<EntityId, CommunityId>,
}

impl CommunityAssignment {
    /// Number of distinct community ids in the assignment.
    pub fn community_count(&self) -> usize {
        let mut ids: Vec<&CommunityId> = self.by_node.values().collect();
        ids.sort_by_key(|c| &c.0);
        ids.dedup();
        ids.len()
    }

    /// All members of the community containing `node`, sorted by EntityId
    /// byte order. Returns empty if `node` is not in the assignment.
    pub fn community_members(&self, node: EntityId) -> Vec<EntityId> {
        let Some(label) = self.by_node.get(&node).copied() else {
            return Vec::new();
        };
        let mut out: Vec<EntityId> = self
            .by_node
            .iter()
            .filter_map(|(n, c)| (*c == label).then_some(*n))
            .collect();
        out.sort_by_key(|e| e.0);
        out
    }
}

/// One label-propagation pass over `graph`, bounded by `max_iters`. Returns a
/// deterministic [`CommunityAssignment`] (same input → same output).
///
/// Empty graphs (no nodes) return an empty assignment. Disconnected
/// subgraphs are detected as separate communities. A clique converges to a
/// single community.
pub fn leiden_pass(graph: &GraphSnapshot, max_iters: usize) -> CommunityAssignment {
    if graph.nodes.is_empty() {
        return CommunityAssignment::default();
    }

    // Deterministic node ordering: dedup + sort by EntityId byte order.
    let mut nodes: Vec<EntityId> = graph.nodes.to_vec();
    nodes.sort_by_key(|e| e.0);
    nodes.dedup();

    // Adjacency map: node → sorted neighbor list (excludes self).
    let mut adj: HashMap<EntityId, Vec<EntityId>> = HashMap::with_capacity(nodes.len());
    for n in &nodes {
        adj.insert(*n, Vec::new());
    }
    for (a, b) in &graph.edges {
        if a == b {
            continue; // self-loops contribute nothing to community formation
        }
        if let Some(v) = adj.get_mut(a) {
            v.push(*b);
        }
        if let Some(v) = adj.get_mut(b) {
            v.push(*a);
        }
    }
    for v in adj.values_mut() {
        v.sort_by_key(|e| e.0);
        v.dedup();
    }

    // Labels: each node starts in its own (integer) community label. Label is
    // an opaque u32 index into `nodes`.
    let mut label: HashMap<EntityId, u32> = HashMap::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        label.insert(*n, i as u32);
    }

    // Iterate until stable or max_iters reached.
    for _ in 0..max_iters {
        let mut changed = false;
        for n in &nodes {
            let neighbors = adj.get(n).expect("every node has an adjacency entry");
            if neighbors.is_empty() {
                continue; // isolated node keeps its singleton label
            }
            // Tally neighbor labels.
            let mut counts: HashMap<u32, u64> = HashMap::new();
            for nb in neighbors {
                let l = *label.get(nb).expect("every neighbor has a label");
                *counts.entry(l).or_insert(0) += 1;
            }
            // Pick the most-frequent label; tie-break by the smallest
            // EntityId.0 representative-node carrying that label among the
            // neighbors. This is fully deterministic.
            let max_count = counts.values().copied().max().unwrap_or(0);
            let candidates: Vec<u32> = counts
                .iter()
                .filter_map(|(l, c)| (*c == max_count).then_some(*l))
                .collect();
            // For each candidate label, find the smallest-EntityId
            // representative AMONG NEIGHBORS carrying that label. Pick the
            // candidate whose representative is the absolute smallest.
            let mut best_label = label[n];
            let mut best_rep_bytes: Option<[u8; 16]> = None;
            for cand in candidates {
                let rep = neighbors
                    .iter()
                    .filter(|nb| label.get(*nb) == Some(&cand))
                    .min_by_key(|e| e.0)
                    .copied();
                if let Some(r) = rep {
                    match best_rep_bytes {
                        None => {
                            best_rep_bytes = Some(r.0);
                            best_label = cand;
                        }
                        Some(prev) if r.0 < prev => {
                            best_rep_bytes = Some(r.0);
                            best_label = cand;
                        }
                        _ => {}
                    }
                }
            }
            if best_label != label[n] {
                label.insert(*n, best_label);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Materialize labels as CommunityIds derived from the sorted member set.
    // Group nodes by integer label first.
    let mut groups: HashMap<u32, Vec<EntityId>> = HashMap::new();
    for n in &nodes {
        groups.entry(label[n]).or_default().push(*n);
    }

    let mut by_node: HashMap<EntityId, CommunityId> = HashMap::with_capacity(nodes.len());
    for (_, members) in groups {
        let cid = CommunityId::from_members(&members);
        for m in members {
            by_node.insert(m, cid);
        }
    }

    CommunityAssignment { by_node }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(n: u8) -> EntityId {
        EntityId([n; 16])
    }

    /// Test 1: empty graph → empty assignment.
    #[test]
    fn empty_graph_returns_empty_assignment() {
        let g = GraphSnapshot::default();
        let a = leiden_pass(&g, 50);
        assert!(a.by_node.is_empty());
        assert_eq!(a.community_count(), 0);
    }

    /// Test 2: two disconnected pairs → two communities.
    /// {1—2} and {3—4}.
    #[test]
    fn two_disconnected_pairs_form_two_communities() {
        let g = GraphSnapshot {
            nodes: vec![entity(1), entity(2), entity(3), entity(4)],
            edges: vec![(entity(1), entity(2)), (entity(3), entity(4))],
        };
        let a = leiden_pass(&g, 50);
        assert_eq!(a.community_count(), 2);
        // Pair (1, 2) shares a community
        assert_eq!(a.by_node[&entity(1)], a.by_node[&entity(2)]);
        // Pair (3, 4) shares a community
        assert_eq!(a.by_node[&entity(3)], a.by_node[&entity(4)]);
        // The two pairs are in different communities
        assert_ne!(a.by_node[&entity(1)], a.by_node[&entity(3)]);
    }

    /// Test 3: triangle clique → one community.
    /// {1—2, 2—3, 1—3} all collapse.
    #[test]
    fn triangle_clique_collapses_to_one_community() {
        let g = GraphSnapshot {
            nodes: vec![entity(1), entity(2), entity(3)],
            edges: vec![
                (entity(1), entity(2)),
                (entity(2), entity(3)),
                (entity(1), entity(3)),
            ],
        };
        let a = leiden_pass(&g, 50);
        assert_eq!(a.community_count(), 1);
        let members = a.community_members(entity(1));
        assert_eq!(members.len(), 3);
    }

    /// Test 4: isolated nodes form singleton communities.
    #[test]
    fn isolated_nodes_form_singletons() {
        let g = GraphSnapshot {
            nodes: vec![entity(1), entity(2), entity(3)],
            edges: vec![],
        };
        let a = leiden_pass(&g, 50);
        assert_eq!(a.community_count(), 3);
        for n in [entity(1), entity(2), entity(3)] {
            assert_eq!(a.community_members(n).len(), 1);
        }
    }

    /// Test 5: determinism — same input produces byte-identical output.
    #[test]
    fn label_propagation_is_deterministic() {
        let g = GraphSnapshot {
            nodes: vec![entity(1), entity(2), entity(3), entity(4), entity(5)],
            edges: vec![
                (entity(1), entity(2)),
                (entity(2), entity(3)),
                (entity(4), entity(5)),
            ],
        };
        let a = leiden_pass(&g, 50);
        let b = leiden_pass(&g, 50);
        assert_eq!(a, b);
    }

    /// Test 6: self-loops are tolerated and don't form communities by
    /// themselves.
    #[test]
    fn self_loops_do_not_create_communities() {
        let g = GraphSnapshot {
            nodes: vec![entity(1), entity(2)],
            edges: vec![
                (entity(1), entity(1)), // self-loop
                (entity(1), entity(2)),
            ],
        };
        let a = leiden_pass(&g, 50);
        assert_eq!(a.community_count(), 1);
        assert_eq!(a.by_node[&entity(1)], a.by_node[&entity(2)]);
    }
}
