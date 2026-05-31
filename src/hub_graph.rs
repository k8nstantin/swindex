//! The hub graph — Layer 2 of the four-layer index.
//!
//! # What the hub graph is
//!
//! Once [`crate::hub::HubSet`] has identified the ~0.1–5% of nodes that
//! are structurally pivotal, the hub graph is the **adjacency among
//! hubs** that the query planner walks for long-range navigation. It
//! is the literal "highway" referred to in the design doc and the
//! Dec 2024 "Hubs in HNSW" paper.
//!
//! # How edges are formed
//!
//! Two hubs are connected in the hub graph if there's a path between
//! them of at most `k_hop` edges in the *underlying* [`crate::Graph`].
//! The edge weight is `1 / hop_distance` — closer hubs get higher
//! weight. This converts shortest-path distance to an edge-weight
//! signal the future query planner can use to prefer shorter routes
//! when multiple paths exist.
//!
//! The construction is a BFS up to depth `k_hop` from each hub; any
//! other hubs encountered become neighbors with weight `1 / d` where
//! `d` is the BFS depth at which they were found.
//!
//! Because the underlying graph is undirected, the BFS distances are
//! symmetric — if `h1` reaches `h2` at depth `d`, the reverse BFS
//! from `h2` reaches `h1` at the same depth. The hub graph is
//! therefore **symmetric by construction**, with the same weight in
//! both directions. We verify this property explicitly in a test
//! rather than relying on it implicitly.
//!
//! # Choosing `k_hop`
//!
//! The design doc (`small-world-index.md` line 102) suggests `k = 3`
//! as a starting point. Empirically:
//!
//! * `k = 1` — only directly adjacent hubs connect. Sparse.
//! * `k = 2` — hubs that share a neighbor connect. Reasonable on
//!   dense graphs.
//! * `k = 3` — the default. On most property graphs, every important
//!   pair of hubs is within 3 hops of each other.
//! * `k ≥ 4` — risks making the hub graph dense (every-hub-to-every-
//!   hub) and losing the navigation signal.
//!
//! Callers can pick any value; this module ships the BFS, not the
//! policy.

use crate::graph::Graph;
use crate::hub::HubSet;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

/// Adjacency among hubs with shortcut-edge weights derived from shortest
/// paths in the underlying [`Graph`]. Layer 2 of the swindex index.
///
/// Built via [`HubGraph::build`]; internal node identities are the same
/// `usize` indices used by [`Graph`] and [`HubSet`], so callers can map
/// between layers without translation.
///
/// Storage is a sorted `BTreeMap` from hub-index to a sorted `Vec` of
/// `(neighbor_hub_idx, weight)` pairs. Iteration order is deterministic
/// across runs.
pub struct HubGraph {
    /// `adj[h]` = sorted neighbors of hub `h` as `(neighbor, weight)`.
    /// Only contains keys for hubs that participate in the hub graph.
    /// Hubs with no other hubs within `k_hop` have an empty entry.
    adj: BTreeMap<usize, Vec<(usize, f64)>>,
}

impl HubGraph {
    /// Construct the hub graph: for every hub in `hubs`, BFS the
    /// underlying [`Graph`] up to depth `k_hop`, and connect to any
    /// other hubs reached along the way. Edge weight = `1 / depth`.
    ///
    /// `k_hop` must be ≥ 1; passing 0 yields an isolated-only hub
    /// graph with every hub present but no edges.
    ///
    /// # Panics
    ///
    /// Panics if any index in `hubs` is out of range for `graph`. The
    /// caller is expected to construct `hubs` from the same `graph`
    /// it was derived from.
    #[must_use]
    pub fn build(graph: &Graph, hubs: &HubSet, k_hop: usize) -> Self {
        // Pre-allocate an empty adjacency entry for every hub. Even
        // hubs with zero neighbors at the chosen `k_hop` should appear
        // in the hub graph so callers can iterate "every hub" cleanly.
        let mut adj: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
        for h in hubs.iter() {
            assert!(
                h < graph.node_count(),
                "hub index {h} out of range for graph of {} nodes",
                graph.node_count()
            );
            adj.insert(h, Vec::new());
        }

        // For each hub, BFS up to depth `k_hop` in the underlying
        // graph and record any other hubs encountered with their
        // hop-distance. Visiting each hub independently is O(H * (V+E))
        // in the worst case but BFS is bounded by the `k_hop` ball so
        // the practical cost is much smaller on sparse graphs.
        for source in hubs.iter() {
            // `depth[v]` = hop distance from `source` to `v`, or
            // missing if not yet visited. Using a `BTreeMap` rather
            // than a Vec so we don't allocate `graph.node_count()`
            // entries per BFS — the ball is typically small.
            let mut depth: BTreeMap<usize, usize> = BTreeMap::new();
            let mut queue: VecDeque<usize> = VecDeque::new();

            depth.insert(source, 0);
            queue.push_back(source);

            while let Some(u) = queue.pop_front() {
                let du = depth[&u];
                if du >= k_hop {
                    // No need to expand further; the BFS ball is
                    // bounded at `k_hop`.
                    continue;
                }
                for (v, _w) in graph.neighbors(u) {
                    if v == u {
                        // Self-loop in the underlying graph — ignore.
                        // Hubs aren't connected to themselves in the
                        // hub graph; the diagonal stays empty.
                        continue;
                    }
                    let dv = du + 1;
                    // Insert only on first visit (BFS gives shortest
                    // distance on first visit thanks to unweighted
                    // breadth ordering).
                    if let std::collections::btree_map::Entry::Vacant(e) = depth.entry(v) {
                        e.insert(dv);
                        if dv < k_hop {
                            queue.push_back(v);
                        }
                    }
                }
            }

            // Now `depth` holds shortest distances from `source` to
            // every node within `k_hop`. Pick out the other hubs.
            let source_neighbors = adj.get_mut(&source).expect("hub entry exists");
            for (&v, &dv) in &depth {
                if v == source {
                    continue;
                }
                if dv == 0 {
                    // Should be unreachable (only source has depth 0)
                    // but guard anyway.
                    continue;
                }
                if hubs.contains(v) {
                    // Convert hop distance into a weight that prefers
                    // short paths. 1/d gives 1.0 at d=1, 0.5 at d=2,
                    // 0.333 at d=3. Float cast is exact for d up to 2^53.
                    #[allow(clippy::cast_precision_loss)]
                    let weight = 1.0_f64 / (dv as f64);
                    source_neighbors.push((v, weight));
                }
            }
            // Keep the neighbor list sorted for deterministic iteration.
            source_neighbors.sort_by_key(|&(v, _)| v);
        }

        Self { adj }
    }

    /// Number of hubs in the hub graph (equal to the size of the
    /// `HubSet` it was built from, regardless of how many edges each
    /// hub ended up with).
    #[must_use]
    pub fn hub_count(&self) -> usize {
        self.adj.len()
    }

    /// `true` iff the hub graph has no hubs at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adj.is_empty()
    }

    /// `true` iff the given node index is a hub participating in this
    /// hub graph.
    #[must_use]
    pub fn contains(&self, hub: usize) -> bool {
        self.adj.contains_key(&hub)
    }

    /// Iterate the neighbors of a given hub as `(neighbor_hub_idx, weight)`
    /// pairs. Returns an empty iterator if `hub` is not present in
    /// the hub graph (rather than panicking).
    #[must_use = "iterator must be consumed"]
    pub fn neighbors(&self, hub: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.adj.get(&hub).map_or([].iter(), |v| v.iter()).copied()
    }

    /// Total number of directed edges (counting both directions of
    /// each symmetric pair, so `2 * undirected_edge_count`).
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.adj.values().map(Vec::len).sum()
    }

    /// Iterate every hub index in the graph, in ascending order.
    #[must_use = "iterator must be consumed"]
    pub fn iter_hubs(&self) -> impl Iterator<Item = usize> + '_ {
        self.adj.keys().copied()
    }
}

impl fmt::Debug for HubGraph {
    // Compact summary — full adjacency is huge in test panic output.
    #[allow(clippy::missing_fields_in_debug)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubGraph")
            .field("hubs", &self.adj.len())
            .field("edges", &self.edge_count())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::HubGraph;
    use crate::graph::Graph;
    use crate::hub::HubSet;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;

    /// Build a path graph: `n` nodes in a chain, edges (0-1), (1-2),
    /// (2-3), ... Useful for testing exact-distance arithmetic.
    fn path_graph(n: usize) -> (Vec<Node>, Vec<Edge>) {
        let nodes: Vec<Node> = (0..n).map(|_| Node::fresh(NodeKind::new("p"))).collect();
        let edges: Vec<Edge> = (0..n.saturating_sub(1))
            .map(|i| Edge::fresh(nodes[i].id, nodes[i + 1].id, EdgeKind::new("e")))
            .collect();
        (nodes, edges)
    }

    #[test]
    fn empty_hub_set_yields_empty_hub_graph() {
        let (nodes, edges) = path_graph(5);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::empty();
        let hg = HubGraph::build(&g, &hubs, 3);
        assert!(hg.is_empty());
        assert_eq!(hg.hub_count(), 0);
        assert_eq!(hg.edge_count(), 0);
    }

    #[test]
    fn single_hub_has_no_neighbors() {
        // One hub, no other hubs to connect to. The hub is present
        // in the hub graph but its neighbor list is empty.
        let (nodes, edges) = path_graph(5);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let only = g.index_of(nodes[2].id).unwrap();
        let hubs: HubSet = [only].into_iter().collect();
        let hg = HubGraph::build(&g, &hubs, 3);
        assert_eq!(hg.hub_count(), 1);
        assert!(hg.contains(only));
        assert_eq!(hg.neighbors(only).count(), 0);
        assert_eq!(hg.edge_count(), 0);
    }

    #[test]
    fn directly_adjacent_hubs_have_weight_one() {
        // Two hubs at the ends of a single edge — distance 1, weight 1.0.
        let (nodes, edges) = path_graph(2);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = g.index_of(nodes[0].id).unwrap();
        let b = g.index_of(nodes[1].id).unwrap();
        let hubs: HubSet = [a, b].into_iter().collect();
        let hg = HubGraph::build(&g, &hubs, 3);

        let a_neighbors: Vec<(usize, f64)> = hg.neighbors(a).collect();
        assert_eq!(a_neighbors, vec![(b, 1.0)]);
        let b_neighbors: Vec<(usize, f64)> = hg.neighbors(b).collect();
        assert_eq!(b_neighbors, vec![(a, 1.0)]);
    }

    #[test]
    fn hubs_with_one_intermediate_have_weight_one_half() {
        // Path 0-1-2: hubs at 0 and 2, intermediate at 1.
        // Hop distance = 2, weight = 0.5.
        let (nodes, edges) = path_graph(3);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = g.index_of(nodes[0].id).unwrap();
        let c = g.index_of(nodes[2].id).unwrap();
        let hubs: HubSet = [a, c].into_iter().collect();
        let hg = HubGraph::build(&g, &hubs, 3);

        let a_neighbors: Vec<(usize, f64)> = hg.neighbors(a).collect();
        assert_eq!(a_neighbors, vec![(c, 0.5)]);
        let c_neighbors: Vec<(usize, f64)> = hg.neighbors(c).collect();
        assert_eq!(c_neighbors, vec![(a, 0.5)]);
    }

    #[test]
    fn hubs_beyond_k_hop_are_not_connected() {
        // Path 0-1-2-3-4: hubs at 0 and 4. Distance 4 > k_hop=3.
        // Hub graph contains both hubs but no edge between them.
        let (nodes, edges) = path_graph(5);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = g.index_of(nodes[0].id).unwrap();
        let e = g.index_of(nodes[4].id).unwrap();
        let hubs: HubSet = [a, e].into_iter().collect();
        let hg = HubGraph::build(&g, &hubs, 3);

        assert_eq!(hg.hub_count(), 2);
        assert_eq!(hg.neighbors(a).count(), 0);
        assert_eq!(hg.neighbors(e).count(), 0);
        assert_eq!(hg.edge_count(), 0);
    }

    #[test]
    fn k_hop_zero_yields_isolated_hubs() {
        // k_hop=0 means "don't explore" — no neighbors discovered.
        let (nodes, edges) = path_graph(3);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = g.index_of(nodes[0].id).unwrap();
        let c = g.index_of(nodes[2].id).unwrap();
        let hubs: HubSet = [a, c].into_iter().collect();
        let hg = HubGraph::build(&g, &hubs, 0);
        assert_eq!(hg.hub_count(), 2);
        assert_eq!(hg.edge_count(), 0);
    }

    #[test]
    fn hub_graph_is_symmetric() {
        // Build the hub graph on Zachary; verify every (a, b) edge
        // has a corresponding (b, a) with the same weight. This is a
        // load-bearing property — the future query planner traverses
        // the hub graph as if undirected.
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::from_top_fraction(&g, 0.10);
        let hg = HubGraph::build(&g, &hubs, 3);

        for h in hg.iter_hubs() {
            for (n, w) in hg.neighbors(h) {
                let reverse: Vec<(usize, f64)> = hg.neighbors(n).collect();
                let found = reverse.iter().find(|&&(x, _)| x == h);
                assert!(
                    found.is_some(),
                    "asymmetry: {h}->{n} weight {w} but reverse missing"
                );
                let (_, rw) = *found.unwrap();
                assert_eq!(rw, w, "asymmetric weights: {h}->{n}={w} vs {n}->{h}={rw}");
            }
        }
    }

    /// Headline test: on Zachary's 34-node graph with 4 top-10% hubs,
    /// every hub reaches every other hub within k_hop=3 because the
    /// graph is small and dense. So the hub graph is complete (4 hubs,
    /// 12 directed edges = 6 undirected).
    #[test]
    fn zachary_hub_graph_is_fully_connected_within_three_hops() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::from_top_fraction(&g, 0.10);
        let hg = HubGraph::build(&g, &hubs, 3);

        assert_eq!(hubs.len(), 4);
        assert_eq!(hg.hub_count(), 4);
        // K_4 has 4 nodes × 3 directed edges per node = 12 entries.
        assert_eq!(hg.edge_count(), 12);
        for h in hubs.iter() {
            assert_eq!(
                hg.neighbors(h).count(),
                3,
                "hub {h} should connect to 3 other hubs"
            );
        }
    }
}
