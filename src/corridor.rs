//! Corridor routing primitives — the in-memory reference
//! implementation behind the future `Connect` query (issue #71,
//! Gate 0 of the Path-A concept validation).
//!
//! # The `Connect` query — specification
//!
//! `Connect { from, to }` answers *"how are X and Y related?"* for two
//! existing nodes. Because Layer 0 lives in the application's own
//! store (swindex persists only structural metadata), the index cannot
//! return an actual L0 path. It returns a **corridor**: an ordered
//! list of clusters bridging `from`'s cluster to `to`'s cluster. The
//! application materializes the real relationship chain by walking
//! only corridor members in its primary store — the sidecar loop
//! applied to point-to-point queries. A good corridor is **small**
//! (the fewer members, the less the application fetches) and
//! **correct** (a true path lies inside it).
//!
//! Correctness has two levels, measured against a BFS oracle on the
//! full graph ([`bfs_distance`]):
//!
//! * **connected** — `from` reaches `to` using only corridor-cluster
//!   nodes. For corridors of consecutively super-graph-adjacent
//!   Leiden clusters this always holds: Leiden guarantees each
//!   cluster is internally connected, and super-adjacency means at
//!   least one bridging edge exists between consecutive clusters.
//! * **optimal** — the corridor preserves the *global* shortest-path
//!   length, i.e. some true shortest path lies entirely inside it.
//!
//! [`cluster_corridor`] is the **baseline arm** of the Gate-1
//! experiment (issue #72): hop-count BFS over the cluster super-graph
//! persisted by issue #70. Gate 1 asks whether hub-graph navigation
//! produces corridors meaningfully *smaller* than this baseline at
//! equal recall — if it can't, the hub highway adds nothing for
//! point-to-point queries either, and the concept folds.
//!
//! `QueryKind::Connect` is deliberately **not** wired into
//! [`crate::SwIndex`] yet — that wiring (issue #73) is gated on a
//! Gate-1 GO. Everything here operates on internal node indices and
//! the in-memory [`Graph`], so the experiment runs without touching
//! persistence.

use crate::community::Partition;
use crate::graph::Graph;
use std::collections::VecDeque;

/// BFS shortest-path distance (hop count) between two node indices on
/// the full graph, or `None` if `to` is unreachable from `from`. The
/// ground-truth oracle corridors are judged against. Edge weights are
/// ignored — corridor quality is about *which* nodes an application
/// must fetch, and hop count is the right yardstick for that.
#[must_use]
pub fn bfs_distance(graph: &Graph, from: usize, to: usize) -> Option<usize> {
    bfs_distance_filtered(graph, from, to, None)
}

/// BFS distance where traversal is restricted to nodes `allowed[v]`
/// (when `allowed` is `Some`). `from` and `to` must themselves be
/// allowed or the search trivially fails.
fn bfs_distance_filtered(
    graph: &Graph,
    from: usize,
    to: usize,
    allowed: Option<&[bool]>,
) -> Option<usize> {
    let n = graph.node_count();
    if from >= n || to >= n {
        return None;
    }
    let permitted = |v: usize| allowed.is_none_or(|a| a[v]);
    if !permitted(from) || !permitted(to) {
        return None;
    }
    if from == to {
        return Some(0);
    }
    // usize::MAX is the "unvisited" sentinel — no graph reaches that
    // many hops, and it keeps the hot loop free of casts.
    let mut dist: Vec<usize> = vec![usize::MAX; n];
    dist[from] = 0;
    let mut queue: VecDeque<usize> = VecDeque::new();
    queue.push_back(from);
    while let Some(v) = queue.pop_front() {
        let dv = dist[v];
        for (w, _) in graph.neighbors(v) {
            if dist[w] == usize::MAX && permitted(w) {
                if w == to {
                    return Some(dv + 1);
                }
                dist[w] = dv + 1;
                queue.push_back(w);
            }
        }
    }
    None
}

/// The cluster-adjacency **baseline corridor**: unweighted BFS over
/// the cluster super-graph from `from_cluster` to `to_cluster`.
/// Returns the ordered cluster path inclusive of both endpoints, or
/// `None` if the clusters are disconnected in the super-graph.
///
/// `adjacency` is [`crate::community::cluster_adjacency`] output (or
/// the persisted `SwIndex::cluster_neighbors` lists, widened).
/// Neighbor lists are sorted ascending by construction and BFS takes
/// the first discovery, so the returned path is deterministic.
///
/// Hop count, not edge mass, drives the search: the corridor's cost
/// to the application is how many clusters (members) it must fetch,
/// which the weights don't measure. Weighted variants belong to the
/// Gate-1 experiment if hop-count corridors prove too coarse.
#[must_use]
pub fn cluster_corridor(
    adjacency: &[Vec<(usize, f64)>],
    from_cluster: usize,
    to_cluster: usize,
) -> Option<Vec<usize>> {
    let k = adjacency.len();
    if from_cluster >= k || to_cluster >= k {
        return None;
    }
    if from_cluster == to_cluster {
        return Some(vec![from_cluster]);
    }
    let mut pred: Vec<Option<usize>> = vec![None; k];
    let mut seen = vec![false; k];
    seen[from_cluster] = true;
    let mut queue: VecDeque<usize> = VecDeque::new();
    queue.push_back(from_cluster);
    'bfs: while let Some(c) = queue.pop_front() {
        for &(d, _) in &adjacency[c] {
            if !seen[d] {
                seen[d] = true;
                pred[d] = Some(c);
                if d == to_cluster {
                    break 'bfs;
                }
                queue.push_back(d);
            }
        }
    }
    if !seen[to_cluster] {
        return None;
    }
    // Walk predecessors back from the target, then reverse.
    let mut path = vec![to_cluster];
    let mut cur = to_cluster;
    while let Some(p) = pred[cur] {
        path.push(p);
        cur = p;
    }
    path.reverse();
    Some(path)
}

/// One corridor judged against the oracle for one `(from, to)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorridorReport {
    /// The corridor's ordered cluster ids, as supplied.
    pub clusters: Vec<usize>,
    /// Total node count across the corridor's clusters — the fetch
    /// bound the application pays. The "size" metric of Gate 1.
    pub member_count: usize,
    /// Global shortest-path hop count between the pair (the oracle).
    pub global_dist: usize,
    /// Shortest-path hop count restricted to corridor members, or
    /// `None` if the pair is disconnected inside the corridor.
    pub corridor_dist: Option<usize>,
}

impl CorridorReport {
    /// `from` reaches `to` without leaving the corridor.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.corridor_dist.is_some()
    }

    /// The corridor preserves the global shortest-path length — some
    /// true shortest path lies entirely inside it. The "recall"
    /// metric of Gate 1.
    #[must_use]
    pub fn optimal(&self) -> bool {
        self.corridor_dist == Some(self.global_dist)
    }
}

/// Judge a corridor (any strategy's output) against the BFS oracle
/// for one `(from, to)` pair. Returns `None` when `to` is globally
/// unreachable from `from` — the corridor question is moot for
/// disconnected pairs and harnesses should skip them.
#[must_use]
pub fn evaluate_corridor(
    graph: &Graph,
    partition: &Partition,
    clusters: &[usize],
    from: usize,
    to: usize,
) -> Option<CorridorReport> {
    let global_dist = bfs_distance(graph, from, to)?;

    let n = graph.node_count();
    let mut in_corridor = vec![false; partition.community_count()];
    for &c in clusters {
        if let Some(slot) = in_corridor.get_mut(c) {
            *slot = true;
        }
    }
    let mut allowed = vec![false; n];
    let mut member_count = 0_usize;
    for v in 0..n {
        if in_corridor[partition.community_of(v)] {
            allowed[v] = true;
            member_count += 1;
        }
    }
    let corridor_dist = bfs_distance_filtered(graph, from, to, Some(&allowed));

    Some(CorridorReport {
        clusters: clusters.to_vec(),
        member_count,
        global_dist,
        corridor_dist,
    })
}

#[cfg(test)]
mod tests {
    use super::{bfs_distance, cluster_corridor, evaluate_corridor};
    use crate::community::{Partition, cluster_adjacency, leiden};
    use crate::graph::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;

    /// `count` triangles in a row, consecutive ones joined by a single
    /// bridge edge (node 0 of triangle i+1 to node 0 of triangle i).
    /// Returns (nodes, edges); triangle t owns indices 3t..3t+3.
    fn triangle_chain(count: usize) -> (Vec<Node>, Vec<Edge>) {
        let mk = |k: &str| Node::fresh(NodeKind::new(k));
        let e = EdgeKind::new("e");
        let nodes: Vec<Node> = (0..3 * count).map(|_| mk("v")).collect();
        let mut edges = Vec::new();
        for t in 0..count {
            let b = 3 * t;
            edges.push(Edge::fresh(nodes[b].id, nodes[b + 1].id, e.clone()));
            edges.push(Edge::fresh(nodes[b + 1].id, nodes[b + 2].id, e.clone()));
            edges.push(Edge::fresh(nodes[b + 2].id, nodes[b].id, e.clone()));
            if t > 0 {
                edges.push(Edge::fresh(nodes[3 * (t - 1)].id, nodes[b].id, e.clone()));
            }
        }
        (nodes, edges)
    }

    fn chain_partition(count: usize) -> Partition {
        let mut labels = Vec::with_capacity(3 * count);
        for t in 0..count {
            labels.extend([t, t, t]);
        }
        Partition::new(labels)
    }

    /// Hand-computed invariant: two bridged triangles. From node 1
    /// (triangle 0) to node 4 (triangle 1) the only shortest path is
    /// 1-0-3-4 (3 hops). The corridor [0, 1] must reproduce exactly
    /// that distance and count all 6 members.
    #[test]
    fn two_bridged_triangles_hand_computed() {
        let (nodes, edges) = triangle_chain(2);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = chain_partition(2);

        let adj = cluster_adjacency(&g, &p);
        let corridor = cluster_corridor(&adj, 0, 1).unwrap();
        assert_eq!(corridor, vec![0, 1]);

        assert_eq!(bfs_distance(&g, 1, 4), Some(3));
        let report = evaluate_corridor(&g, &p, &corridor, 1, 4).unwrap();
        assert_eq!(report.member_count, 6);
        assert_eq!(report.global_dist, 3);
        assert_eq!(report.corridor_dist, Some(3));
        assert!(report.connected());
        assert!(report.optimal());
    }

    /// A three-cluster chain A-B-C: the corridor from A to C must pass
    /// through B (the super-graph has no A-C edge), and dropping B
    /// from the corridor must disconnect the pair — that's what the
    /// `connected` metric detects.
    #[test]
    fn three_cluster_chain_requires_the_middle() {
        let (nodes, edges) = triangle_chain(3);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = chain_partition(3);

        let adj = cluster_adjacency(&g, &p);
        let corridor = cluster_corridor(&adj, 0, 2).unwrap();
        assert_eq!(corridor, vec![0, 1, 2]);

        let full = evaluate_corridor(&g, &p, &corridor, 1, 7).unwrap();
        assert!(full.connected() && full.optimal());
        assert_eq!(full.member_count, 9);

        // Same pair, corridor missing the middle cluster: globally a
        // path exists, inside the gutted corridor it must not.
        let gutted = evaluate_corridor(&g, &p, &[0, 2], 1, 7).unwrap();
        assert!(!gutted.connected());
        assert!(!gutted.optimal());
    }

    /// Disconnected clusters: no corridor exists in the super-graph,
    /// and the oracle skips globally-unreachable pairs.
    #[test]
    fn disconnected_clusters_yield_no_corridor() {
        let (nodes, mut edges) = triangle_chain(2);
        // Remove the bridge (the last edge pushed connects the two
        // triangles).
        edges.pop();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = chain_partition(2);

        let adj = cluster_adjacency(&g, &p);
        assert_eq!(cluster_corridor(&adj, 0, 1), None);
        // Globally unreachable pair → evaluate_corridor declines.
        assert_eq!(evaluate_corridor(&g, &p, &[0, 1], 0, 3), None);
        assert_eq!(bfs_distance(&g, 0, 3), None);
    }

    /// Same-cluster pairs degenerate gracefully: a single-cluster
    /// corridor that is trivially optimal.
    #[test]
    fn same_cluster_pair_is_a_single_cluster_corridor() {
        let (nodes, edges) = triangle_chain(2);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = chain_partition(2);

        let adj = cluster_adjacency(&g, &p);
        let corridor = cluster_corridor(&adj, 1, 1).unwrap();
        assert_eq!(corridor, vec![1]);
        let report = evaluate_corridor(&g, &p, &corridor, 3, 5).unwrap();
        assert!(report.optimal());
        assert_eq!(report.member_count, 3);
    }

    /// Real-graph invariant on Zachary with a Leiden partition: the
    /// baseline corridor is connected for EVERY cross-cluster pair.
    /// This is a theorem, not a tendency — Leiden clusters are
    /// internally connected (tested elsewhere) and consecutive
    /// corridor clusters share at least one super-edge — so a single
    /// failure means a routing bug, not a hard graph.
    #[test]
    fn zachary_baseline_corridor_always_connected() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let p = leiden(&g);
        let adj = cluster_adjacency(&g, &p);

        let n = g.node_count();
        let mut pairs = 0_usize;
        let mut optimal = 0_usize;
        for from in 0..n {
            for to in (from + 1)..n {
                let (cf, ct) = (p.community_of(from), p.community_of(to));
                if cf == ct {
                    continue;
                }
                let corridor = cluster_corridor(&adj, cf, ct)
                    .unwrap_or_else(|| panic!("no corridor {cf}->{ct} on connected Zachary"));
                let report =
                    evaluate_corridor(&g, &p, &corridor, from, to).expect("Zachary is connected");
                assert!(
                    report.connected(),
                    "corridor {corridor:?} disconnects pair ({from},{to})"
                );
                pairs += 1;
                if report.optimal() {
                    optimal += 1;
                }
            }
        }
        assert!(
            pairs > 100,
            "expected many cross-cluster pairs, got {pairs}"
        );
        // Optimal-rate floor: deterministic given the fixed Leiden
        // seed, measured at well above this; the floor guards against
        // silent routing regressions without pinning the exact value.
        #[allow(clippy::cast_precision_loss)]
        let rate = optimal as f64 / pairs as f64;
        assert!(
            rate >= 0.70,
            "baseline optimal rate regressed: {optimal}/{pairs} = {rate:.3}"
        );
    }
}
