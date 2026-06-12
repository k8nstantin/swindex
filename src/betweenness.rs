//! Approximate betweenness centrality — Brandes' algorithm with sampled
//! sources (issue #23).
//!
//! # Why this exists
//!
//! Degree-based hub detection ([`crate::hub::HubSet::from_top_fraction`])
//! finds the *busy* nodes — the ones with many incident edges. But the
//! structurally pivotal nodes for navigation are often the **bridges**:
//! nodes that sit on the shortest path between two otherwise-separate
//! communities, even when their own degree is modest. A single node
//! joining two dense clusters has degree 2 but lies on *every* path
//! between them — degree-detection misses it entirely, betweenness finds
//! it immediately.
//!
//! This is the gap the v0.1 review flagged as "the single algorithmic
//! gap that most undercuts the headline claim": without betweenness, the
//! hub graph's highway can have holes exactly where the long-range
//! shortcuts should be.
//!
//! # The algorithm
//!
//! Brandes (2001) computes betweenness for every node in `O(V·E)` on
//! unweighted graphs — a dramatic improvement over the naive
//! all-pairs-shortest-paths `O(V³)`. The core innovation is the
//! **dependency accumulation**: a single BFS from each source `s`
//! computes, for every other node, the number of shortest `s`-paths
//! (`σ`) and the BFS predecessors; then a reverse pass over the BFS
//! order accumulates each node's dependency `δ` in one sweep.
//!
//! We **sample** sources rather than running from all `V` of them. With
//! `k` sampled sources the cost drops to `O(k·(V+E))` and the estimate
//! converges fast — Bader/Kintali/Madduri/Mihail (2007) show
//! `k = O((log V)/ε²)` sources give tight bounds. For the hub-detection
//! use-case we only need the *ranking* to be right, which sampling
//! preserves at far smaller `k` than tight absolute bounds require.
//!
//! # Unweighted, simple-graph path counting
//!
//! The BFS treats the underlying [`Graph`] as **unweighted and simple**:
//!
//! * Edge weights are ignored — Brandes' `O(V·E)` bound is the
//!   *unweighted* one (weighted betweenness needs Dijkstra and an extra
//!   `log V`). Current swindex sources emit unit-weight edges anyway.
//! * **Parallel edges are collapsed.** The procedure-co-occurrence
//!   source ([`crate::sql_dump::SqlDumpSource`]) emits a pair co-mentioned
//!   in *N* routines as *N* separate unit edges. Left uncollapsed those
//!   would multiply the shortest-path count `σ` and distort the
//!   dependency math. We dedupe each node's neighbor set before the BFS
//!   so path counting is over the simple graph.
//! * **Self-loops are dropped** — a node's loop never lies on a shortest
//!   path between two *distinct* nodes.
//!
//! # Determinism
//!
//! Source selection is driven by a seeded xorshift64 RNG (the same PRNG
//! the rest of the crate uses), so `approximate_betweenness(g, k, seed)`
//! returns byte-identical scores across runs. When `samples >= V` every
//! node is used as a source (exact Brandes) and the seed is irrelevant.

use crate::graph::Graph;
use std::collections::VecDeque;

/// Approximate betweenness-centrality scores for every node in `graph`,
/// indexed by internal node id (`result[i]` is the score of node `i`).
///
/// `samples` is the number of BFS source nodes to run from:
///
/// * `samples == 0` → returns an all-zero vector (no estimation
///   requested). Callers that rank by the result get index order.
/// * `0 < samples < V` → samples that many distinct sources via the
///   seeded RNG; the returned scores are scaled by `V / samples` so
///   magnitudes are comparable to the exact run. (For pure ranking the
///   scale is irrelevant.)
/// * `samples >= V` → uses every node as a source: this is **exact**
///   Brandes betweenness, and `seed` has no effect.
///
/// The scores are *not* normalized to `[0, 1]` and do not apply the
/// undirected `/2` convention — they're relative magnitudes intended
/// for ranking (which hub detection is), not for cross-graph comparison
/// against published normalized betweenness values.
///
/// # Complexity
///
/// `O(samples · (V + E))` time, `O(V + E)` working memory per source.
#[must_use]
pub fn approximate_betweenness(graph: &Graph, samples: usize, seed: u64) -> Vec<f64> {
    let n = graph.node_count();
    let mut centrality = vec![0.0_f64; n];
    if n == 0 {
        return centrality;
    }

    // Distinct-neighbor adjacency: collapse parallel edges and drop
    // self-loops so shortest-path counting is over the simple graph
    // (see module doc — parallel edges from the proc-co-occurrence
    // source would otherwise inflate σ).
    let adj: Vec<Vec<usize>> = (0..n)
        .map(|u| {
            let mut ns: Vec<usize> = graph
                .neighbors(u)
                .map(|(v, _)| v)
                .filter(|&v| v != u)
                .collect();
            ns.sort_unstable();
            ns.dedup();
            ns
        })
        .collect();

    let sources = pick_sources(n, samples, seed);
    let k = sources.len();
    if k == 0 {
        return centrality;
    }

    // Per-source scratch buffers, reused across sources to avoid
    // reallocating `O(V)` vectors `k` times.
    let mut sigma = vec![0.0_f64; n];
    let mut dist = vec![-1_i64; n];
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut delta = vec![0.0_f64; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut queue: VecDeque<usize> = VecDeque::new();

    for &s in &sources {
        // Reset only what the previous source touched would be ideal,
        // but a full reset is O(V) which is dominated by the BFS's
        // O(V+E) anyway. Clarity over micro-optimization here.
        for v in 0..n {
            sigma[v] = 0.0;
            dist[v] = -1;
            preds[v].clear();
            delta[v] = 0.0;
        }
        order.clear();
        queue.clear();

        // Phase 1: BFS from s, counting shortest paths (σ) and
        // recording predecessors along shortest paths.
        sigma[s] = 1.0;
        dist[s] = 0;
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            order.push(v);
            let dv = dist[v];
            for &w in &adj[v] {
                if dist[w] < 0 {
                    dist[w] = dv + 1;
                    queue.push_back(w);
                }
                // w is one hop further along a shortest path through v.
                if dist[w] == dv + 1 {
                    sigma[w] += sigma[v];
                    preds[w].push(v);
                }
            }
        }

        // Phase 2: accumulate dependencies in reverse BFS order. Each
        // node hands its accumulated dependency back to its shortest-
        // path predecessors, weighted by their share of the σ count.
        while let Some(w) = order.pop() {
            let coeff = (1.0 + delta[w]) / sigma[w];
            for &v in &preds[w] {
                delta[v] += sigma[v] * coeff;
            }
            if w != s {
                centrality[w] += delta[w];
            }
        }
    }

    // Scale the sampled estimate up to a full-graph estimate so the
    // magnitudes don't depend on how many sources we happened to run.
    // Exact runs (k == n) are already full-scale.
    if k < n {
        #[allow(clippy::cast_precision_loss)]
        let scale = n as f64 / k as f64;
        for c in &mut centrality {
            *c *= scale;
        }
    }

    centrality
}

/// Choose the BFS source set deterministically.
///
/// * `samples == 0` → empty (caller gets all-zero centrality).
/// * `samples >= n` → all nodes (exact Brandes).
/// * otherwise → `samples` distinct nodes via a partial Fisher-Yates
///   shuffle driven by the seeded RNG, then sorted so the accumulation
///   order is itself reproducible.
fn pick_sources(n: usize, samples: usize, seed: u64) -> Vec<usize> {
    if samples == 0 {
        return Vec::new();
    }
    if samples >= n {
        return (0..n).collect();
    }
    let mut idx: Vec<usize> = (0..n).collect();
    let mut state = if seed == 0 { 1 } else { seed };
    // Partial Fisher-Yates: fully shuffle only the first `samples` slots.
    for i in 0..samples {
        let r = xorshift64(&mut state);
        #[allow(clippy::cast_possible_truncation)]
        let j = i + (r as usize) % (n - i);
        idx.swap(i, j);
    }
    idx.truncate(samples);
    // Sort so the per-source iteration order is stable independent of
    // the shuffle's internal ordering — keeps results byte-identical.
    idx.sort_unstable();
    idx
}

/// xorshift64 — small, fast, deterministic. Duplicated from
/// `community.rs` rather than shared; review #44 §4.6 tracks promoting
/// one copy to `pub(crate)`. Kept local here to avoid widening
/// `community`'s surface as a side effect of this PR.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::approximate_betweenness;
    use crate::graph::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;

    /// Two cliques of `clique_size` nodes joined by a single bridge node
    /// connected to one node in each clique. The bridge node has the
    /// **lowest** degree (2) but the **highest** betweenness (it's on
    /// every path between the two cliques). Returns the nodes, edges,
    /// and the bridge node's index in `nodes`.
    fn bridge_of_two_cliques(clique_size: usize) -> (Vec<Node>, Vec<Edge>, usize) {
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();

        // Clique A: indices 0..clique_size.
        for _ in 0..clique_size {
            nodes.push(Node::fresh(NodeKind::new("a")));
        }
        // Clique B: indices clique_size..2*clique_size.
        for _ in 0..clique_size {
            nodes.push(Node::fresh(NodeKind::new("b")));
        }
        // Bridge node: index 2*clique_size.
        let bridge_idx = 2 * clique_size;
        nodes.push(Node::fresh(NodeKind::new("bridge")));

        let kind = EdgeKind::new("e");
        // Fully connect clique A.
        for i in 0..clique_size {
            for j in (i + 1)..clique_size {
                edges.push(Edge::fresh(nodes[i].id, nodes[j].id, kind.clone()));
            }
        }
        // Fully connect clique B.
        for i in clique_size..2 * clique_size {
            for j in (i + 1)..2 * clique_size {
                edges.push(Edge::fresh(nodes[i].id, nodes[j].id, kind.clone()));
            }
        }
        // Bridge connects a-node 0 and b-node 0 to the bridge node.
        edges.push(Edge::fresh(nodes[0].id, nodes[bridge_idx].id, kind.clone()));
        edges.push(Edge::fresh(
            nodes[clique_size].id,
            nodes[bridge_idx].id,
            kind.clone(),
        ));

        (nodes, edges, bridge_idx)
    }

    #[test]
    fn empty_graph_yields_empty_scores() {
        let src = SliceSource::new(&[], &[]);
        let g = Graph::from_source(&src).unwrap();
        let scores = approximate_betweenness(&g, 10, 42);
        assert!(scores.is_empty());
    }

    #[test]
    fn zero_samples_yields_all_zero_scores() {
        let (nodes, edges, _) = bridge_of_two_cliques(4);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let scores = approximate_betweenness(&g, 0, 42);
        assert_eq!(scores.len(), g.node_count());
        assert!(scores.iter().all(|&s| s == 0.0));
    }

    /// The defining test for this module: on a graph where the bridge
    /// node has the *lowest* degree but lies on every inter-clique path,
    /// betweenness must rank it #1 while degree ranks it last. This is
    /// exactly the structure degree-based detection cannot see.
    #[test]
    fn bridge_node_has_max_betweenness_but_not_max_degree() {
        let (nodes, edges, bridge_local) = bridge_of_two_cliques(4);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let bridge = g.index_of(nodes[bridge_local].id).unwrap();

        // Exact betweenness (samples >= n).
        let scores = approximate_betweenness(&g, g.node_count(), 0);

        // Bridge is the unique argmax of betweenness.
        let max_idx = (0..g.node_count())
            .max_by(|&a, &b| scores[a].partial_cmp(&scores[b]).unwrap())
            .unwrap();
        assert_eq!(
            max_idx, bridge,
            "bridge node {bridge} should have the highest betweenness, got {max_idx}"
        );

        // Bridge has the *lowest* degree (2) — degree detection would
        // never pick it.
        let bridge_degree = g.degree(bridge);
        assert_eq!(bridge_degree, 2.0);
        for i in 0..g.node_count() {
            if i != bridge {
                assert!(
                    g.degree(i) > bridge_degree,
                    "every clique node should out-degree the bridge"
                );
            }
        }
    }

    #[test]
    fn exact_run_is_seed_independent() {
        // When samples >= n every node is a source, so the seed can't
        // matter — the two runs must be byte-identical.
        let (nodes, edges, _) = bridge_of_two_cliques(5);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = approximate_betweenness(&g, g.node_count(), 1);
        let b = approximate_betweenness(&g, g.node_count(), 999);
        assert_eq!(a, b);
    }

    #[test]
    fn sampled_run_is_deterministic_for_a_fixed_seed() {
        // On Zachary with a sub-V sample, the same seed must reproduce
        // the same scores exactly.
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let a = approximate_betweenness(&g, 10, 12345);
        let b = approximate_betweenness(&g, 10, 12345);
        assert_eq!(a, b);
        // A different seed should (almost surely) pick a different
        // source set and yield different scores — guards against the
        // seed being silently ignored.
        let c = approximate_betweenness(&g, 10, 67890);
        assert_ne!(a, c);
    }

    /// On Zachary, exact betweenness must rank Mr. Hi (node 0) and the
    /// Officer (node 33) — the two community leaders — among the very
    /// top. They're both the highest-degree AND highest-betweenness
    /// nodes here, which is the "hubs are high-degree" regime degree
    /// detection handles fine; this test confirms betweenness agrees
    /// rather than contradicts in that regime.
    #[test]
    fn zachary_leaders_top_betweenness() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let scores = approximate_betweenness(&g, g.node_count(), 0);

        // The two highest-degree nodes (the published leaders, degrees
        // 16 and 17) must both land in the top 4 by betweenness.
        let mut by_deg: Vec<(usize, f64)> = (0..g.node_count()).map(|i| (i, g.degree(i))).collect();
        by_deg.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let leader1 = by_deg[0].0;
        let leader2 = by_deg[1].0;

        let mut by_btw: Vec<(usize, f64)> = (0..g.node_count()).map(|i| (i, scores[i])).collect();
        by_btw.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let top4: Vec<usize> = by_btw.iter().take(4).map(|&(i, _)| i).collect();

        assert!(
            top4.contains(&leader1) && top4.contains(&leader2),
            "both community leaders ({leader1}, {leader2}) should be top-4 by betweenness; top4 = {top4:?}"
        );
    }
}
