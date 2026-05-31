//! Region graph — Layer 3 of the four-layer index.
//!
//! # What a region is
//!
//! A **region** is a group of clusters that the query planner treats
//! as a unit at the top of the routing hierarchy. The progression is:
//!
//! ```text
//! Layer 0  full fact graph     (nodes, edges)
//! Layer 1  cluster graph       (Leiden communities of nodes)
//! Layer 2  hub graph           (subset of nodes that are pivotal)
//! Layer 3  region graph        (Leiden communities of CLUSTERS)
//! ```
//!
//! A query first asks "which region is this about?" (one cheap lookup),
//! then narrows to the relevant hubs in that region, then expands to
//! the clusters those hubs anchor, then to the nodes in those clusters.
//! Without Layer 3, queries that span the substrate uniformly would
//! degrade to scanning every cluster.
//!
//! # How regions are detected
//!
//! Recursive Leiden: collapse each cluster (Layer 1) into a single
//! super-node, weight the edges between super-nodes by the sum of
//! inter-cluster edge weights in the original graph, then run Leiden
//! again on that smaller super-graph. The partition Leiden produces
//! is the cluster→region mapping.
//!
//! This is the same trick Microsoft GraphRAG uses for offline
//! hierarchical summarization (arxiv:2404.16130). swindex applies it
//! online for query routing — region detection is fast (the super-
//! graph has thousands of nodes, not billions) and the result lives
//! alongside the cluster partition in storage.
//!
//! # What this module ships
//!
//! [`RegionGraph`] — wraps the cluster→region partition with a few
//! convenience accessors so callers don't have to remember "regions
//! live on the same `Partition` shape but over a different index space."
//! Construction delegates to [`crate::community::regions_from_clusters`].

use crate::community::{Partition, regions_from_clusters};
use crate::graph::Graph;
use std::fmt;

/// The cluster → region mapping plus a few accessors.
///
/// Internally a [`Partition`] over cluster indices. Region ids are
/// renumbered to a contiguous `0..r` range.
pub struct RegionGraph {
    /// `cluster_to_region.community_of(cluster_id)` = region id.
    /// `cluster_to_region.node_count()` = number of clusters.
    /// `cluster_to_region.community_count()` = number of regions.
    cluster_to_region: Partition,
}

impl RegionGraph {
    /// Build the region graph from a graph and its cluster partition.
    ///
    /// `clusters` must be a partition over the nodes of `graph` (e.g.
    /// the output of [`crate::leiden`]). Returns a `RegionGraph` whose
    /// region ids cover `[0, region_count)`.
    ///
    /// Uses the default Leiden seed (42) for the recursive pass. For
    /// reproducibility with a different seed use
    /// [`RegionGraph::build_seeded`].
    #[must_use]
    pub fn build(graph: &Graph, clusters: &Partition) -> Self {
        Self::build_seeded(graph, clusters, 42)
    }

    /// Build with an explicit seed for the recursive Leiden pass.
    #[must_use]
    pub fn build_seeded(graph: &Graph, clusters: &Partition, seed: u64) -> Self {
        Self {
            cluster_to_region: regions_from_clusters(graph, clusters, seed),
        }
    }

    /// Region id of a given cluster.
    ///
    /// Returns `None` if `cluster_id` is out of range for this region
    /// graph (e.g. you passed a cluster id from a different partition).
    #[must_use]
    pub fn region_of_cluster(&self, cluster_id: usize) -> Option<usize> {
        if cluster_id >= self.cluster_to_region.node_count() {
            None
        } else {
            Some(self.cluster_to_region.community_of(cluster_id))
        }
    }

    /// Number of distinct regions (always in `0..region_count()`).
    #[must_use]
    pub fn region_count(&self) -> usize {
        self.cluster_to_region.community_count()
    }

    /// Number of clusters this region graph was built from.
    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.cluster_to_region.node_count()
    }

    /// Group clusters by region. `result[r]` is the list of cluster
    /// ids belonging to region `r`.
    #[must_use]
    pub fn clusters_by_region(&self) -> Vec<Vec<usize>> {
        self.cluster_to_region.buckets()
    }

    /// Iterate the region id of every cluster, in cluster-index order.
    #[must_use = "iterator must be consumed"]
    pub fn iter_assignments(&self) -> impl Iterator<Item = usize> + '_ {
        self.cluster_to_region.iter()
    }
}

impl fmt::Debug for RegionGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegionGraph")
            .field("clusters", &self.cluster_count())
            .field("regions", &self.region_count())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::RegionGraph;
    use crate::community::{Partition, leiden, regions_from_clusters};
    use crate::graph::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;

    fn triangle() -> (Vec<Node>, Vec<Edge>) {
        let a = Node::fresh(NodeKind::new("v"));
        let b = Node::fresh(NodeKind::new("v"));
        let c = Node::fresh(NodeKind::new("v"));
        let edges = vec![
            Edge::fresh(a.id, b.id, EdgeKind::new("e")),
            Edge::fresh(b.id, c.id, EdgeKind::new("e")),
            Edge::fresh(c.id, a.id, EdgeKind::new("e")),
        ];
        (vec![a, b, c], edges)
    }

    fn two_disjoint_triangles() -> (Vec<Node>, Vec<Edge>) {
        let (mut n1, e1) = triangle();
        let (n2, e2) = triangle();
        n1.extend(n2);
        let edges: Vec<Edge> = e1.into_iter().chain(e2).collect();
        (n1, edges)
    }

    #[test]
    fn empty_partition_yields_empty_region_graph() {
        let src = SliceSource::new(&[], &[]);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(Vec::new());
        let r = RegionGraph::build(&g, &p);
        assert_eq!(r.cluster_count(), 0);
        assert_eq!(r.region_count(), 0);
        assert!(r.region_of_cluster(0).is_none());
    }

    #[test]
    fn single_cluster_yields_single_region() {
        // A triangle with all 3 nodes in one cluster: 1 cluster → 1 region.
        let (nodes, edges) = triangle();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0]);
        let r = RegionGraph::build(&g, &p);
        assert_eq!(r.cluster_count(), 1);
        assert_eq!(r.region_count(), 1);
        assert_eq!(r.region_of_cluster(0), Some(0));
    }

    #[test]
    fn two_disjoint_clusters_with_no_inter_edges_yield_two_regions() {
        // Two disjoint triangles → 2 clusters → no inter-cluster edges in
        // the super-graph → each cluster is its own region (no merging
        // possible).
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);
        let r = RegionGraph::build(&g, &p);
        assert_eq!(r.cluster_count(), 2);
        assert_eq!(r.region_count(), 2);
        // Each cluster gets its own region; the renumbering is
        // contiguous so the two regions are 0 and 1.
        assert_ne!(
            r.region_of_cluster(0).unwrap(),
            r.region_of_cluster(1).unwrap()
        );
    }

    #[test]
    fn out_of_range_cluster_returns_none() {
        let (nodes, edges) = triangle();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0]);
        let r = RegionGraph::build(&g, &p);
        assert!(r.region_of_cluster(999).is_none());
    }

    #[test]
    fn is_deterministic_for_a_fixed_seed() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);
        let a = RegionGraph::build_seeded(&g, &p, 1234);
        let b = RegionGraph::build_seeded(&g, &p, 1234);
        assert_eq!(a.cluster_count(), b.cluster_count());
        assert_eq!(a.region_count(), b.region_count());
        for c in 0..a.cluster_count() {
            assert_eq!(a.region_of_cluster(c), b.region_of_cluster(c));
        }
    }

    /// Headline structural test: Zachary's 4 Leiden communities are
    /// fed through recursive Leiden. The result might or might not
    /// merge them — on Zachary specifically, the 4 clusters are
    /// well-balanced enough that the modularity-optimal partition
    /// keeps each cluster in its own region (the cluster super-graph's
    /// inter-cluster edges aren't strong enough for merging to improve
    /// modularity). That's a valid result: it means cluster-level
    /// detail is the right granularity for queries on Zachary's scale,
    /// and the Layer-3 region graph just passes through.
    ///
    /// We assert: every cluster gets a region, the region count is in
    /// `[1, 4]`, and the bucket structure adds up. We don't assert a
    /// specific region count because the answer depends on the cluster
    /// super-graph's edge weights, which depend on how Leiden split
    /// the original — those upstream choices can shift slightly across
    /// seeds and still be correct.
    #[test]
    fn regions_on_zachary_partitions_the_4_clusters() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let clusters = leiden(&g);
        assert_eq!(clusters.community_count(), 4);

        let r = RegionGraph::build(&g, &clusters);
        assert_eq!(r.cluster_count(), 4);
        let n_regions = r.region_count();
        assert!(
            (1..=4).contains(&n_regions),
            "expected 1..=4 regions for 4 Zachary clusters, got {n_regions}"
        );

        // Every cluster must be assigned to some region.
        for c in 0..4 {
            assert!(r.region_of_cluster(c).is_some());
        }

        // Bucket structure adds up.
        let buckets = r.clusters_by_region();
        assert_eq!(buckets.len(), n_regions);
        let total: usize = buckets.iter().map(Vec::len).sum();
        assert_eq!(total, 4);
    }

    /// Cross-check: the bare `regions_from_clusters` function and the
    /// `RegionGraph::build` wrapper produce the same partition.
    #[test]
    fn wrapper_matches_bare_function() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);
        let bare = regions_from_clusters(&g, &p, 42);
        let wrapped = RegionGraph::build_seeded(&g, &p, 42);
        for c in 0..p.community_count() {
            assert_eq!(bare.community_of(c), wrapped.region_of_cluster(c).unwrap());
        }
    }
}
