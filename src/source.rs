//! The boundary between "where the graph lives" and "the index that consumes it."
//!
//! A [`GraphSource`] is anything that can hand the index a stream of [`Node`]s
//! and a stream of [`Edge`]s. The index doesn't care whether the source is a
//! slice in memory, a GML file on disk, a `petgraph::Graph<N, E>`, or a
//! streaming feed from an application layer — only that it can iterate them.
//!
//! Two reference sources ship in the crate today:
//!
//! - [`SliceSource`] — wraps `&[Node]` and `&[Edge]`. Used in tests and tiny
//!   in-memory builds.
//! - more to come in subsequent releases (`GmlSource` for SNAP datasets,
//!   `PetgraphSource` for callers that already have an in-memory graph).
//!
//! The trait yields owned [`Node`]/[`Edge`] values rather than references so
//! that streaming and lazy sources (decoding off disk, materializing rows on
//! the fly) can implement it without holding everything in memory. In-memory
//! sources just clone — the per-node cost is dominated by Leiden's later
//! work, so this is cheap in practice.

use crate::node::{Edge, Node};

/// A producer of graph nodes and edges for the index to consume.
///
/// Implementors guarantee:
///
/// 1. Calling [`Self::nodes`] yields every node in the source, exactly once,
///    in any order.
/// 2. Calling [`Self::edges`] yields every edge in the source, exactly once,
///    in any order.
/// 3. Both methods can be called more than once and produce the same sequence
///    (up to ordering). A source that consumes its underlying stream must
///    buffer or otherwise re-create the sequence on each call.
///
/// The size hints are advisory and used to pre-size index buffers. Returning
/// `None` is always correct; returning a tight upper bound is best.
pub trait GraphSource {
    /// Iterate every node in the source.
    fn nodes(&self) -> impl Iterator<Item = Node> + '_;

    /// Iterate every edge in the source.
    fn edges(&self) -> impl Iterator<Item = Edge> + '_;

    /// Optional hint for the total number of nodes; defaults to `None`.
    fn node_count_hint(&self) -> Option<usize> {
        None
    }

    /// Optional hint for the total number of edges; defaults to `None`.
    fn edge_count_hint(&self) -> Option<usize> {
        None
    }
}

/// A [`GraphSource`] backed by two slices held in memory.
///
/// `SliceSource` is the simplest possible source. It does not validate that
/// edges refer to nodes that exist in the node slice; that's the index
/// builder's responsibility (a later PR will surface dangling-edge errors at
/// build time).
#[derive(Debug, Clone, Copy)]
pub struct SliceSource<'a> {
    nodes: &'a [Node],
    edges: &'a [Edge],
}

impl<'a> SliceSource<'a> {
    /// Wrap a pair of slices.
    #[must_use]
    pub const fn new(nodes: &'a [Node], edges: &'a [Edge]) -> Self {
        Self { nodes, edges }
    }

    /// Number of nodes in the underlying slice.
    #[must_use]
    pub const fn node_len(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the underlying slice.
    #[must_use]
    pub const fn edge_len(&self) -> usize {
        self.edges.len()
    }
}

impl GraphSource for SliceSource<'_> {
    fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        self.nodes.iter().cloned()
    }

    fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.edges.iter().cloned()
    }

    fn node_count_hint(&self) -> Option<usize> {
        Some(self.nodes.len())
    }

    fn edge_count_hint(&self) -> Option<usize> {
        Some(self.edges.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{GraphSource, SliceSource};
    use crate::node::{Edge, EdgeKind, Node, NodeKind};

    fn make_demo() -> (Vec<Node>, Vec<Edge>) {
        let parcel = Node::fresh(NodeKind::new("parcel"));
        let owner = Node::fresh(NodeKind::new("owner"));
        let nodes = vec![parcel.clone(), owner.clone()];
        let edges = vec![Edge::fresh(owner.id, parcel.id, EdgeKind::new("owns"))];
        (nodes, edges)
    }

    #[test]
    fn empty_source_yields_nothing() {
        let src = SliceSource::new(&[], &[]);
        assert_eq!(src.nodes().count(), 0);
        assert_eq!(src.edges().count(), 0);
        assert_eq!(src.node_count_hint(), Some(0));
        assert_eq!(src.edge_count_hint(), Some(0));
    }

    #[test]
    fn yields_exactly_the_input() {
        let (nodes, edges) = make_demo();
        let src = SliceSource::new(&nodes, &edges);

        let collected_nodes: Vec<_> = src.nodes().collect();
        let collected_edges: Vec<_> = src.edges().collect();
        assert_eq!(collected_nodes, nodes);
        assert_eq!(collected_edges, edges);
    }

    #[test]
    fn iteration_is_repeatable() {
        let (nodes, edges) = make_demo();
        let src = SliceSource::new(&nodes, &edges);

        let first: Vec<_> = src.nodes().collect();
        let second: Vec<_> = src.nodes().collect();
        assert_eq!(
            first, second,
            "calling nodes() twice must return the same sequence"
        );

        let first_e: Vec<_> = src.edges().collect();
        let second_e: Vec<_> = src.edges().collect();
        assert_eq!(first_e, second_e);
    }

    #[test]
    fn size_hints_match_slice_lengths() {
        let (nodes, edges) = make_demo();
        let src = SliceSource::new(&nodes, &edges);
        assert_eq!(src.node_count_hint(), Some(2));
        assert_eq!(src.edge_count_hint(), Some(1));
        assert_eq!(src.node_len(), 2);
        assert_eq!(src.edge_len(), 1);
    }

    /// Confirms that `GraphSource` works as a generic bound, the way the
    /// future `SwIndex::build_from_source(source: impl GraphSource)` will use
    /// it. If this fn ever stops compiling, the trait shape has regressed.
    fn count_via_trait<G: GraphSource>(g: &G) -> (usize, usize) {
        (g.nodes().count(), g.edges().count())
    }

    #[test]
    fn works_through_generic_trait_bound() {
        let (nodes, edges) = make_demo();
        let src = SliceSource::new(&nodes, &edges);
        assert_eq!(count_via_trait(&src), (2, 1));
    }
}
