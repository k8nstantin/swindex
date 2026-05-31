//! Internal in-memory graph representation used by the clustering algorithms.
//!
//! # Why a separate type, not just a `Vec<Edge>`
//!
//! The clustering algorithms (Louvain in this PR, Leiden in the next)
//! need to ask three questions hundreds of thousands of times per run:
//!
//! 1. "What are node *u*'s neighbors, and what's the edge weight to each?"
//! 2. "What's the total weight of edges incident to *u*?" (its degree)
//! 3. "What's the total edge weight in the graph?" (2*m in the modularity
//!    formula)
//!
//! A flat `Vec<Edge>` answers (1) in O(E) and (2) in O(E) — neither is
//! tolerable inside the algorithm's inner loop. [`Graph`] precomputes
//! both during construction and serves them in O(1) per lookup.
//!
//! # Undirected, weighted, with self-loops permitted
//!
//! The clustering algorithms work on undirected graphs. swindex's
//! public [`crate::Edge`] type is directed (it has a `source` → `target`
//! orientation, with its own id), but for clustering purposes we collapse
//! every directed edge into an undirected one — each call to [`Graph::add_edge`]
//! adds the edge in both directions to the adjacency list.
//!
//! This is conservative: if your underlying data really is directed
//! (citation graph, web graph), folding both directions effectively
//! treats them as the same connection. That's the right choice for
//! community detection — node A links to node B is structurally
//! equivalent to node B linking to node A from the perspective of
//! "are they in the same community?"
//!
//! Self-loops (edges from a node to itself) are permitted and
//! contribute their full weight to the node's degree once, not twice.
//! This is consistent with the modularity formula's handling.
//!
//! # Internal indexing
//!
//! Nodes are stored in a `Vec<NodeId>` and addressed internally by
//! `usize` index for cache efficiency. The mapping from public
//! [`crate::NodeId`] (a [`Uuid7`]) to internal index lives in a
//! `BTreeMap` for deterministic iteration.
//!
//! [`Uuid7`]: crate::Uuid7

use crate::node::NodeId;
use crate::source::GraphSource;
use std::collections::BTreeMap;
use std::fmt;

/// An undirected, weighted in-memory graph.
///
/// Built from a [`GraphSource`] via [`Graph::from_source`]. Once
/// constructed, immutable from the caller's perspective — the
/// algorithms in [`crate::community`] only read from it.
///
/// Storage is two flat vectors plus a sorted map:
///
/// * `nodes[i]` — the public [`NodeId`] of internal node *i*.
/// * `adj[i]` — the adjacency list of internal node *i*: a `Vec` of
///   `(neighbor_index, edge_weight)` pairs.
/// * `node_to_idx` — reverse lookup so the source's `NodeId` values
///   can be translated to internal indices.
///
/// Plus three precomputed scalars:
///
/// * `degrees[i]` — sum of edge weights incident to *i*.
/// * `twice_m` — twice the total edge weight (the `2m` term in the
///   modularity formula). Stored doubled so the formula doesn't have
///   to multiply at each evaluation.
/// * `loop_weight[i]` — self-loop weight on *i*, if any. Tracked
///   separately because the standard adjacency-list traversal would
///   visit it twice (once as outgoing, once as incoming) but it should
///   only contribute once to a node's degree.
pub struct Graph {
    /// Public node ids indexed by internal `usize`.
    nodes: Vec<NodeId>,
    /// Reverse lookup: public id -> internal index. `BTreeMap` rather
    /// than `HashMap` so iteration order is deterministic.
    node_to_idx: BTreeMap<NodeId, usize>,
    /// `adj[i]` = neighbors of node `i` as `(neighbor_idx, weight)`.
    /// Symmetric — every edge appears in both endpoints' lists.
    adj: Vec<Vec<(usize, f64)>>,
    /// `degrees[i]` = sum of edge weights touching node `i`, with
    /// self-loops counted exactly once.
    degrees: Vec<f64>,
    /// `loop_weight[i]` = weight of node `i`'s self-loop, if any.
    /// Used by the modularity calculation to avoid double-counting.
    loop_weight: Vec<f64>,
    /// Twice the total undirected edge weight in the graph. This is
    /// the `2m` denominator that appears all over the modularity
    /// formula and Louvain's delta-Q computation.
    twice_m: f64,
}

/// Errors that can occur while building a [`Graph`] from a [`GraphSource`].
#[derive(Debug)]
pub enum GraphError {
    /// An edge referenced a `source` or `target` `NodeId` that the
    /// source didn't yield in its `nodes()` iterator. This is the
    /// "dangling edge" condition swindex catches at build time rather
    /// than letting it produce nonsense partitions later.
    DanglingEdge { which: NodeId },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::DanglingEdge { which } => {
                write!(f, "edge endpoint not present in source's node set: {which}")
            }
        }
    }
}

impl std::error::Error for GraphError {}

impl Graph {
    /// Build a [`Graph`] from any [`GraphSource`].
    ///
    /// Iterates the source's nodes once and edges once. Each edge is
    /// added in both directions (the graph is undirected — see module
    /// docs for the rationale). Edge weights default to `1.0`; future
    /// PRs may extend [`GraphSource`] to expose per-edge weights.
    ///
    /// # Errors
    ///
    /// * [`GraphError::DanglingEdge`] — an edge referenced a `NodeId`
    ///   that the source's `nodes()` iterator didn't yield.
    pub fn from_source<G: GraphSource>(source: &G) -> Result<Self, GraphError> {
        // Phase 1: walk every node, assign internal indices, allocate
        // adjacency lists. Using BTreeMap so the eventual index order
        // is deterministic — tests rely on it.
        let mut nodes: Vec<NodeId> = Vec::new();
        let mut node_to_idx: BTreeMap<NodeId, usize> = BTreeMap::new();
        for n in source.nodes() {
            // Duplicate ids in the source would corrupt the mapping;
            // we silently keep the first occurrence's index, matching
            // the convention used by every academic graph dataset
            // (Zachary, SNAP) — duplicate node lines are de-facto
            // expected to be deduplicated by the loader.
            node_to_idx.entry(n.id).or_insert_with(|| {
                let idx = nodes.len();
                nodes.push(n.id);
                idx
            });
        }
        let n_nodes = nodes.len();
        let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n_nodes];
        let mut degrees = vec![0.0_f64; n_nodes];
        let mut loop_weight = vec![0.0_f64; n_nodes];
        let mut twice_m = 0.0_f64;

        // Phase 2: walk every edge, resolve endpoints, add to both
        // adjacency lists. Edge weight is currently always 1.0 — when
        // GraphSource exposes weights, swap the constant for the
        // source-provided value.
        for e in source.edges() {
            let weight = 1.0_f64;
            let s = *node_to_idx
                .get(&e.source)
                .ok_or(GraphError::DanglingEdge { which: e.source })?;
            let t = *node_to_idx
                .get(&e.target)
                .ok_or(GraphError::DanglingEdge { which: e.target })?;
            if s == t {
                // Self-loop. Per the modularity formula's standard
                // handling, it contributes its weight to the degree
                // exactly once (not twice as a back-edge would).
                adj[s].push((t, weight));
                degrees[s] += weight;
                loop_weight[s] += weight;
                twice_m += weight;
            } else {
                // Symmetric add: both endpoints see each other.
                adj[s].push((t, weight));
                adj[t].push((s, weight));
                degrees[s] += weight;
                degrees[t] += weight;
                // Each undirected edge contributes 2*weight to "2m"
                // because every node's degree counts the edge once,
                // and we sum all degrees.
                twice_m += 2.0 * weight;
            }
        }

        Ok(Self {
            nodes,
            node_to_idx,
            adj,
            degrees,
            loop_weight,
            twice_m,
        })
    }

    /// Number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Public [`NodeId`] of internal node `i`, panicking on an out-of-range
    /// index. Used by callers that need to map a partition back to the
    /// caller's id space.
    #[must_use]
    pub fn node_id(&self, i: usize) -> NodeId {
        self.nodes[i]
    }

    /// Internal index of a public [`NodeId`], or `None` if not present.
    #[must_use]
    pub fn index_of(&self, id: NodeId) -> Option<usize> {
        self.node_to_idx.get(&id).copied()
    }

    /// `2m` — twice the total undirected edge weight. The denominator
    /// in the modularity formula.
    #[must_use]
    pub fn twice_total_weight(&self) -> f64 {
        self.twice_m
    }

    /// Sum of edge weights incident to node `i`. Self-loops contribute
    /// once (consistent with the modularity formula's convention).
    #[must_use]
    pub fn degree(&self, i: usize) -> f64 {
        self.degrees[i]
    }

    /// Self-loop weight on node `i`, if any. Used by the modularity
    /// formula to avoid double-counting a node's loop.
    #[must_use]
    pub fn self_loop(&self, i: usize) -> f64 {
        self.loop_weight[i]
    }

    /// Iterate node `i`'s neighbors as `(neighbor_idx, weight)` pairs.
    pub fn neighbors(&self, i: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        // `.copied()` produces owned `(usize, f64)` pairs so callers
        // don't deal with references inside hot loops.
        self.adj[i].iter().copied()
    }
}

// Manual Debug — we deliberately don't dump the full adjacency list,
// node_to_idx, or per-node arrays since they balloon test panic output
// without adding diagnostic value. `clippy::missing_fields_in_debug`
// flags the omission; this is intentional.
#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph")
            .field("nodes", &self.nodes.len())
            .field("edges_x2", &self.adj.iter().map(Vec::len).sum::<usize>())
            .field("twice_m", &self.twice_m)
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Hand-computed expected values are exact.
mod tests {
    use super::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;

    /// Build a 4-node graph with edges (0-1), (1-2), (2-3), (0-2).
    /// Total m = 4, so twice_m = 8.
    /// Degrees: node 0=2, node 1=2, node 2=3, node 3=1.
    fn diamond_graph() -> (Vec<Node>, Vec<Edge>) {
        let n0 = Node::fresh(NodeKind::new("v"));
        let n1 = Node::fresh(NodeKind::new("v"));
        let n2 = Node::fresh(NodeKind::new("v"));
        let n3 = Node::fresh(NodeKind::new("v"));
        let edges = vec![
            Edge::fresh(n0.id, n1.id, EdgeKind::new("e")),
            Edge::fresh(n1.id, n2.id, EdgeKind::new("e")),
            Edge::fresh(n2.id, n3.id, EdgeKind::new("e")),
            Edge::fresh(n0.id, n2.id, EdgeKind::new("e")),
        ];
        (vec![n0, n1, n2, n3], edges)
    }

    #[test]
    fn empty_graph_has_zero_total_weight() {
        let src = SliceSource::new(&[], &[]);
        let g = Graph::from_source(&src).unwrap();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.twice_total_weight(), 0.0);
    }

    #[test]
    fn diamond_has_expected_structure() {
        let (nodes, edges) = diamond_graph();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();

        assert_eq!(g.node_count(), 4);
        // 4 undirected edges => 2m = 8 (each edge contributes 2 to the sum)
        assert_eq!(g.twice_total_weight(), 8.0);

        // Degree assertions — node indices come from BTreeMap iteration,
        // which is deterministic but doesn't necessarily preserve the
        // order of the input `nodes` Vec (BTreeMap sorts by NodeId).
        // We look up by id to be robust to that.
        let i0 = g.index_of(nodes[0].id).unwrap();
        let i1 = g.index_of(nodes[1].id).unwrap();
        let i2 = g.index_of(nodes[2].id).unwrap();
        let i3 = g.index_of(nodes[3].id).unwrap();
        assert_eq!(g.degree(i0), 2.0);
        assert_eq!(g.degree(i1), 2.0);
        assert_eq!(g.degree(i2), 3.0);
        assert_eq!(g.degree(i3), 1.0);
    }

    #[test]
    fn adjacency_is_symmetric() {
        // Every (u, v) in the adjacency of u must have u in the
        // adjacency of v. Catches silent direction bugs.
        let (nodes, edges) = diamond_graph();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();

        for u in 0..g.node_count() {
            for (v, w) in g.neighbors(u) {
                let v_has_u = g
                    .neighbors(v)
                    .any(|(x, xw)| x == u && (xw - w).abs() < 1e-12);
                assert!(v_has_u, "asymmetry: {u}->{v} present, reverse missing");
            }
        }
    }

    #[test]
    fn dangling_edge_errors() {
        // An edge whose endpoints aren't in the node slice must fail
        // the build — silently dropping it would let the algorithms
        // produce a partition over a graph that's smaller than what
        // the source actually described.
        let n0 = Node::fresh(NodeKind::new("v"));
        let n1 = Node::fresh(NodeKind::new("v"));
        let nodes = vec![n0.clone()]; // n1 deliberately absent
        let edges = vec![Edge::fresh(n0.id, n1.id, EdgeKind::new("e"))];
        let src = SliceSource::new(&nodes, &edges);

        let err = Graph::from_source(&src).unwrap_err();
        match err {
            super::GraphError::DanglingEdge { which } => assert_eq!(which, n1.id),
        }
    }
}
