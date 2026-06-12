//! Hub detection — the second layer of the four-layer swindex index.
//!
//! # Why hubs matter
//!
//! Real property graphs are not flat. A small fraction of nodes — high-
//! degree connectors, type-anchor nodes, institutional registries — sit
//! at structurally pivotal positions and serve as the "highway" through
//! which most multi-hop traversals route. Identifying these hubs and
//! navigating through them first is the difference between O(N) and
//! O(log N) query complexity.
//!
//! The December 2024 paper *"Down with the Hierarchy: The 'H' in HNSW
//! Stands for Hubs"* (arxiv:2412.01940) argued that HNSW's effectiveness
//! comes not from its layered structure but from the **hubs that emerge
//! from random small-world insertion**. swindex generalizes that insight
//! from vectors to property graphs: identify hubs explicitly (rather
//! than relying on emergence) and route queries through them first.
//!
//! # What this module ships
//!
//! [`HubSet`] — a set of internal node indices flagged as hubs. Built
//! by:
//!
//! * [`HubSet::from_top_fraction`] — top *k* % of nodes by degree.
//!   E.g. `from_top_fraction(g, 0.1)` returns the 10% highest-degree
//!   nodes. The simplest detection criterion and the right default for
//!   most graphs.
//! * [`HubSet::from_degree_threshold`] — every node with degree ≥ τ.
//!   Used when you have an absolute degree target (e.g. "anyone with
//!   at least 100 incident facts is a hub").
//! * [`HubSet::from_centrality`] — top *k* % by **approximate
//!   betweenness centrality** (Brandes' algorithm, issue #23). Finds
//!   the *bridge* nodes — low-degree connectors between communities —
//!   that the degree criteria above structurally cannot see. See
//!   [`crate::betweenness`] for the algorithm.
//! * [`HubSet::empty`] / [`HubSet::from_iter`] — for tests and callers
//!   that want to construct a hub set manually.
//!
//! # What this module deliberately doesn't do yet
//!
//! * **Type-based hub eligibility** — flagging every node whose
//!   `NodeKind` is in a configured `HUB_TYPES` set (e.g. registries,
//!   institutional anchors). Trivial to add once consumers actually
//!   need it.
//! * **The hub graph** — adjacency among hubs with weighted "shortcut"
//!   edges. That's the Layer-2 structure the query planner traverses.
//!   Coming next.

use crate::graph::Graph;
use std::cmp::Ordering;
use std::collections::BTreeSet;

/// A set of internal node indices flagged as hubs of a [`Graph`].
///
/// Hubs are stored as `usize` indices into the source graph rather
/// than as [`crate::Uuid7`]s — the caller can map back via
/// [`Graph::node_id`] when public ids are needed. Indexing by `usize`
/// keeps the per-hub footprint tiny and makes `contains` checks O(log N)
/// via `BTreeSet`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubSet {
    /// Sorted set of hub indices. `BTreeSet` rather than `HashSet` so
    /// iteration order is deterministic — tests rely on it.
    hubs: BTreeSet<usize>,
}

impl FromIterator<usize> for HubSet {
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        Self {
            hubs: iter.into_iter().collect(),
        }
    }
}

impl HubSet {
    /// An empty hub set.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a `HubSet` from a slice of node indices. The slice can
    /// contain duplicates; only distinct indices are retained.
    ///
    /// For an iterator-based constructor use the [`FromIterator`] impl:
    /// `HubSet::from_iter(some_iter)` works as expected.
    #[must_use]
    pub fn from_indices(indices: &[usize]) -> Self {
        Self {
            hubs: indices.iter().copied().collect(),
        }
    }

    /// Identify hubs as the top `fraction` of nodes by degree.
    ///
    /// `fraction` is clamped to `[0.0, 1.0]`. A value of `0.0` returns
    /// an empty set; `1.0` returns every node. The expected operational
    /// range is `0.001..=0.05` (0.1%–5%) — large enough to capture all
    /// pivotal nodes, small enough that the hub graph remains tiny
    /// relative to the full graph.
    ///
    /// Ties are broken by lower internal index — i.e., among nodes with
    /// equal degree, the earlier-minted ones are preferred. This is
    /// deterministic given a fixed graph; the choice of tie-breaker is
    /// arbitrary but stable.
    ///
    /// Runs in `O(N log N)` (sort the degrees) which is negligible
    /// compared to Leiden's cost on the same graph.
    #[must_use]
    pub fn from_top_fraction(graph: &Graph, fraction: f64) -> Self {
        let n = graph.node_count();
        if n == 0 {
            return Self::empty();
        }
        // Clamp into the valid range. NaN clamps to 0 because the
        // comparison f.is_nan() is true; floor of NaN is NaN which
        // would propagate to a 0-sized count below.
        let frac = fraction.clamp(0.0, 1.0);
        // Round up so a graph of 10 nodes and 5% fraction still yields
        // 1 hub (rather than truncating to 0). The precision-loss cast
        // is fine here — node counts above 2^53 (the f64 integer-exact
        // range) are not a real concern.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let k = ((n as f64) * frac).ceil() as usize;
        let k = k.min(n);
        if k == 0 {
            return Self::empty();
        }

        // Build (idx, degree) pairs and partial-sort to find the top k.
        // For graphs up to ~10^6 nodes the full sort is fine; if/when
        // this becomes a bottleneck we can switch to `select_nth_unstable`
        // which is O(N).
        let mut indexed: Vec<(usize, f64)> = (0..n).map(|i| (i, graph.degree(i))).collect();
        indexed.sort_by(|a, b| {
            // Descending by degree; tie-break ascending by index so the
            // result is deterministic and the same across runs.
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        indexed.into_iter().take(k).map(|(i, _)| i).collect()
    }

    /// Identify hubs as the top `top_fraction` of nodes by
    /// **approximate betweenness centrality** (Brandes' algorithm with
    /// `samples` sampled BFS sources; see
    /// [`crate::betweenness::approximate_betweenness`]).
    ///
    /// Where [`Self::from_top_fraction`] finds the *busy* (high-degree)
    /// nodes, this finds the *pivotal* ones — bridges that lie on the
    /// shortest paths between communities even when their own degree is
    /// modest. On graphs whose hubs are simply the high-degree nodes the
    /// two criteria largely agree; on graphs with low-degree bridge
    /// nodes (a single connector between two dense clusters) only the
    /// centrality criterion finds them.
    ///
    /// `top_fraction` is clamped to `[0.0, 1.0]` and the cut count is
    /// rounded **up** (so a non-zero fraction always yields at least one
    /// hub), exactly matching [`Self::from_top_fraction`]'s semantics.
    /// Ties in centrality are broken by lower internal index, so the
    /// result is deterministic given `(graph, samples, top_fraction,
    /// seed)`.
    ///
    /// Pass `samples >= graph.node_count()` for exact betweenness (seed
    /// then has no effect); pass a smaller `samples` to trade accuracy
    /// for the `O(samples · (V+E))` runtime.
    #[must_use]
    pub fn from_centrality(graph: &Graph, samples: usize, top_fraction: f64, seed: u64) -> Self {
        let n = graph.node_count();
        if n == 0 {
            return Self::empty();
        }
        let frac = top_fraction.clamp(0.0, 1.0);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let k = ((n as f64) * frac).ceil() as usize;
        let k = k.min(n);
        if k == 0 {
            return Self::empty();
        }

        let scores = crate::betweenness::approximate_betweenness(graph, samples, seed);
        let mut indexed: Vec<(usize, f64)> = (0..n).map(|i| (i, scores[i])).collect();
        indexed.sort_by(|a, b| {
            // Descending by centrality; tie-break ascending by index so
            // the result is deterministic — same convention as
            // `from_top_fraction`.
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        indexed.into_iter().take(k).map(|(i, _)| i).collect()
    }

    /// Identify hubs as every node whose degree is `>= min_degree`.
    ///
    /// Use when you have an absolute degree target — e.g. "anyone with
    /// at least 100 incident facts is a hub" — rather than a relative
    /// percentile. The two criteria can give very different hub sets
    /// on graphs with skewed degree distributions; pick the one whose
    /// semantics match your application.
    #[must_use]
    pub fn from_degree_threshold(graph: &Graph, min_degree: f64) -> Self {
        let hubs: BTreeSet<usize> = (0..graph.node_count())
            .filter(|&i| graph.degree(i) >= min_degree)
            .collect();
        Self { hubs }
    }

    /// `true` iff the given internal node index is a hub in this set.
    #[must_use]
    pub fn contains(&self, node_idx: usize) -> bool {
        self.hubs.contains(&node_idx)
    }

    /// Number of hubs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hubs.len()
    }

    /// `true` iff no nodes are flagged as hubs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hubs.is_empty()
    }

    /// Iterate the hub indices in ascending order. Deterministic across
    /// runs thanks to the `BTreeSet` backing store.
    #[must_use = "iterator must be consumed"]
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.hubs.iter().copied()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::HubSet;
    use crate::graph::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;

    /// Build a "star" graph: one central node connected to `spokes`
    /// leaf nodes. The center has degree `spokes`; each leaf has
    /// degree 1. Useful for testing that the highest-degree node is
    /// definitely picked up.
    fn star_graph(spokes: usize) -> (Vec<Node>, Vec<Edge>) {
        let center = Node::fresh(NodeKind::new("hub"));
        let mut nodes = vec![center.clone()];
        let mut edges = Vec::new();
        for _ in 0..spokes {
            let leaf = Node::fresh(NodeKind::new("leaf"));
            edges.push(Edge::fresh(center.id, leaf.id, EdgeKind::new("connects")));
            nodes.push(leaf);
        }
        (nodes, edges)
    }

    #[test]
    fn empty_graph_has_no_hubs() {
        let src = SliceSource::new(&[], &[]);
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::from_top_fraction(&g, 0.10);
        assert!(hubs.is_empty());
        assert_eq!(hubs.len(), 0);
    }

    #[test]
    fn zero_fraction_yields_empty_set() {
        let (nodes, edges) = star_graph(10);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::from_top_fraction(&g, 0.0);
        assert!(hubs.is_empty());
    }

    #[test]
    fn full_fraction_yields_every_node() {
        let (nodes, edges) = star_graph(10);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::from_top_fraction(&g, 1.0);
        assert_eq!(hubs.len(), g.node_count());
        for i in 0..g.node_count() {
            assert!(hubs.contains(i));
        }
    }

    #[test]
    fn small_fraction_rounds_up_to_at_least_one() {
        // 10 nodes, fraction 0.01 → would round down to 0; we round
        // up to 1 to avoid the surprising "no hubs at all" case.
        let (nodes, edges) = star_graph(9); // 10 total nodes
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let hubs = HubSet::from_top_fraction(&g, 0.01);
        assert_eq!(hubs.len(), 1);
    }

    #[test]
    fn star_center_is_the_first_hub() {
        // In a star graph, the center has the highest degree by far.
        // It must be in any non-empty hub set.
        let (nodes, edges) = star_graph(20);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();

        // The center is the first node in `nodes`, so its internal
        // index is 0 (BTreeMap-ordered by Uuid7, which is monotonic
        // in mint order).
        let center_idx = g.index_of(nodes[0].id).unwrap();
        assert_eq!(g.degree(center_idx), 20.0);

        let hubs = HubSet::from_top_fraction(&g, 0.10);
        assert!(
            hubs.contains(center_idx),
            "star center must be in the top 10% by degree"
        );
    }

    #[test]
    fn degree_threshold_filters_by_absolute_value() {
        let (nodes, edges) = star_graph(5);
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        // Threshold = 3 means only the center (degree 5) qualifies.
        let hubs = HubSet::from_degree_threshold(&g, 3.0);
        assert_eq!(hubs.len(), 1);
        // Threshold = 0 means everyone (since every leaf has degree 1
        // and 1 >= 0).
        let all = HubSet::from_degree_threshold(&g, 0.0);
        assert_eq!(all.len(), g.node_count());
    }

    #[test]
    fn iter_is_sorted() {
        let hubs: HubSet = [5, 1, 9, 3, 1].into_iter().collect();
        let collected: Vec<usize> = hubs.iter().collect();
        assert_eq!(collected, vec![1, 3, 5, 9]);
    }

    #[test]
    fn from_indices_deduplicates() {
        let hubs = HubSet::from_indices(&[2, 2, 5, 5, 5, 1]);
        assert_eq!(hubs.len(), 3);
        let collected: Vec<usize> = hubs.iter().collect();
        assert_eq!(collected, vec![1, 2, 5]);
    }

    /// Headline test: degree-based hub detection on Zachary karate
    /// must pick up Mr. Hi and the Officer — the two highest-degree
    /// nodes in the graph (degrees 16 and 17). Top 10% of 34 = 4 hubs.
    #[test]
    fn zachary_top_hubs_include_mr_hi_and_officer() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("member"),
            &EdgeKind::new("friendship"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        assert_eq!(g.node_count(), 34);

        // Find the two highest-degree nodes by hand (we know from the
        // fixture they're the first and last gml ids).
        let mut by_deg: Vec<(usize, f64)> = (0..g.node_count()).map(|i| (i, g.degree(i))).collect();
        by_deg.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let top1 = by_deg[0].0;
        let top2 = by_deg[1].0;
        // Sanity: degrees match the published numbers (16 and 17).
        assert!(
            by_deg[0].1 >= 16.0,
            "expected top degree >= 16, got {}",
            by_deg[0].1
        );

        let hubs = HubSet::from_top_fraction(&g, 0.10);
        // Top 10% of 34 ceil = 4 hubs.
        assert_eq!(
            hubs.len(),
            4,
            "expected 4 hubs at top 10%, got {}",
            hubs.len()
        );
        assert!(
            hubs.contains(top1) && hubs.contains(top2),
            "the two highest-degree nodes must be in the hub set"
        );
    }

    /// Issue #23 acceptance criterion: on Zachary, the centrality-based
    /// hub set must overlap heavily (≥ 80%) with the degree-based one.
    /// Zachary is the "hubs are high-degree" regime, so the two
    /// criteria should mostly agree — this guards against a betweenness
    /// implementation that ranks nonsense.
    #[test]
    fn zachary_centrality_hubs_agree_with_degree_hubs() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("member"),
            &EdgeKind::new("friendship"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();

        // Compare top-third sets (34 → 12 nodes) for a meaningful
        // overlap statistic — top-10% (4 nodes) is too small to express
        // an 80% threshold cleanly.
        let frac = 1.0 / 3.0;
        let degree_hubs = HubSet::from_top_fraction(&g, frac);
        // Exact betweenness (samples >= n) so the test is seed-stable.
        let centrality_hubs = HubSet::from_centrality(&g, g.node_count(), frac, 0);

        assert_eq!(
            degree_hubs.len(),
            centrality_hubs.len(),
            "both criteria round to the same cut size"
        );
        let k = degree_hubs.len();
        let overlap = centrality_hubs
            .iter()
            .filter(|&i| degree_hubs.contains(i))
            .count();
        #[allow(clippy::cast_precision_loss)]
        let agreement = overlap as f64 / k as f64;
        assert!(
            agreement >= 0.80,
            "centrality/degree hub agreement on Zachary was {agreement:.2} ({overlap}/{k}); expected ≥ 0.80"
        );
    }

    /// A graph where the two criteria *disagree*: a low-degree bridge
    /// node between two cliques is a centrality hub but not a degree
    /// hub. This is the structure that justifies betweenness existing.
    #[test]
    fn bridge_node_is_a_centrality_hub_but_not_a_degree_hub() {
        // Two 5-cliques joined through a single degree-2 bridge node.
        let mut nodes: Vec<Node> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();
        let kind = EdgeKind::new("e");
        for _ in 0..10 {
            nodes.push(Node::fresh(NodeKind::new("clique")));
        }
        let bridge = Node::fresh(NodeKind::new("bridge"));
        // Clique A = 0..5, clique B = 5..10.
        for (lo, hi) in [(0, 5), (5, 10)] {
            for i in lo..hi {
                for j in (i + 1)..hi {
                    edges.push(Edge::fresh(nodes[i].id, nodes[j].id, kind.clone()));
                }
            }
        }
        edges.push(Edge::fresh(nodes[0].id, bridge.id, kind.clone()));
        edges.push(Edge::fresh(nodes[5].id, bridge.id, kind.clone()));
        nodes.push(bridge.clone());

        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let bridge_idx = g.index_of(bridge.id).unwrap();

        // Top single node by each criterion.
        let degree_hub = HubSet::from_top_fraction(&g, 0.05); // ceil(11*0.05)=1
        let centrality_hub = HubSet::from_centrality(&g, g.node_count(), 0.05, 0);
        assert_eq!(degree_hub.len(), 1);
        assert_eq!(centrality_hub.len(), 1);

        assert!(
            centrality_hub.contains(bridge_idx),
            "betweenness must pick the bridge node as the #1 hub"
        );
        assert!(
            !degree_hub.contains(bridge_idx),
            "degree must NOT pick the (lowest-degree) bridge node"
        );
    }
}
