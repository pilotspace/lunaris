//! Plan 04-02 Task 2 — Leiden community detection (D-15 Option A: hand-rolled
//! label-propagation). Rustworkx is explicitly rejected because it carries
//! unsafe blocks and would violate `#![forbid(unsafe_code)]`.
//!
//! B-7 forward-compat stub: Task 1 commits this file with the public types
//! and [`leiden_pass`] signature so `cargo check` passes after Task 1 alone.
//! Task 2 replaces the body with the real label-propagation pass + the 6
//! unit tests.

use std::collections::HashMap;

use lunaris_extract::EntityId;

use crate::types::CommunityId;

/// A minimal graph snapshot used by [`leiden_pass`]. Plan 04-04 wires the
/// umbrella handle to build one of these from the `StoragePort::graph_traverse`
/// result; Plan 04-02 keeps the shape pure-data so unit tests can build tiny
/// graphs without a live backend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSnapshot {
    /// Nodes in the graph. Must be unique (no duplicates).
    pub nodes: Vec<EntityId>,
    /// Undirected edges (a, b). The label-propagation pass treats each edge as
    /// bidirectional; self-loops are allowed but contribute nothing to
    /// community formation.
    pub edges: Vec<(EntityId, EntityId)>,
}

/// Output of one label-propagation pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommunityAssignment {
    /// Map from EntityId → CommunityId. Every node in the input graph appears
    /// exactly once; isolated nodes form singleton communities.
    pub by_node: HashMap<EntityId, CommunityId>,
}

/// One label-propagation pass over `graph`, bounded by `max_iters` (Task 2 body).
pub fn leiden_pass(_graph: &GraphSnapshot, _max_iters: usize) -> CommunityAssignment {
    unimplemented!("Task 2 replaces this stub with hand-rolled label-propagation (D-15 Option A)")
}
