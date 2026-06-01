//! The boundary between "where the graph lives" and "the index that consumes it."
//!
//! # Why a trait, not a concrete type
//!
//! The index has to consume graphs from a wide variety of sources:
//!
//! * In-memory slices (tests, small fixtures)
//! * GML / GraphML / EdgeList files on disk (SNAP datasets, NetworkX
//!   exports, benchmark graphs)
//! * `petgraph::Graph<N, E>` values (callers that already have a graph)
//! * Rows streaming from a SQL cursor over MySQL / Postgres
//! * Arrow `RecordBatch`es from Iceberg / Parquet
//! * A live changefeed from another swindex instance (replication)
//! * An HTTP API that yields one node-or-edge per request (rare, but
//!   needed for some federation patterns)
//!
//! None of these can be expressed as a single concrete type. They have
//! different lifetimes, different error modes, different cost models for
//! "iterate the same thing twice." A trait lets each source satisfy
//! exactly the contract the builder needs without committing to a
//! particular representation.
//!
//! # The contract
//!
//! [`GraphSource`] guarantees three things — see the trait doc for the
//! exact wording. Briefly: `nodes()` yields every node exactly once,
//! `edges()` yields every edge exactly once, and either method may be
//! called repeatedly and produces the same sequence (modulo ordering).
//!
//! That third clause matters more than it looks. The build path iterates
//! nodes once (to assign ids to clusters), then iterates edges several
//! times (once per Leiden pass), and we cannot afford the iterator to
//! consume an underlying stream. Sources whose backing data is a true
//!
//! stream (a SQL cursor, a Kafka topic) must buffer or rewind internally
//! to satisfy the repeatability clause.
//!
//! # Owned values, not borrowed
//!
//! The trait yields owned `Node` / `Edge` values rather than references.
//! This is a deliberate design choice — it lets streaming sources
//! materialize values on the fly without buffering the whole graph in
//! memory as a long-lived `Vec`. In-memory sources just clone — the
//! per-item clone cost is negligible compared to Leiden's later work.
//!
//! If a hot path ever shows the clones in a profile, we can add a
//! parallel `for_each_node` / `for_each_edge` API to the trait without
//! breaking existing implementors.

use crate::node::{Edge, Node};

/// A producer of graph nodes and edges for the index to consume.
///
/// # Contract
///
/// Implementors guarantee:
///
/// 1. Calling [`Self::nodes`] yields every node in the source, exactly
///    once, in any order.
/// 2. Calling [`Self::edges`] yields every edge in the source, exactly
///    once, in any order.
/// 3. Either method can be called more than once and produces the same
///    sequence (up to ordering). A source whose backing data is a true
///    stream must buffer or rewind internally to satisfy this clause —
///    the index builder iterates each method several times during a
///    single build.
///
/// # Size hints
///
/// [`Self::node_count_hint`] and [`Self::edge_count_hint`] are advisory
/// and used to pre-size the index's internal buffers. Returning `None`
/// is always correct; returning a tight upper bound is best. The
/// builder treats hints as `Vec::with_capacity` arguments, not as
/// contracts — under-counting wastes a few reallocations, over-counting
/// wastes a few bytes of memory, neither causes a bug.
pub trait GraphSource {
    /// Iterate every node in the source.
    ///
    /// Must satisfy clauses (1) and (3) of the trait contract.
    fn nodes(&self) -> impl Iterator<Item = Node> + '_;

    /// Iterate every edge in the source.
    ///
    /// Must satisfy clauses (2) and (3) of the trait contract.
    fn edges(&self) -> impl Iterator<Item = Edge> + '_;

    /// Optional hint for the total number of nodes; defaults to `None`.
    fn node_count_hint(&self) -> Option<usize> {
        None
    }

    /// Optional hint for the total number of edges; defaults to `None`.
    fn edge_count_hint(&self) -> Option<usize> {
        None
    }

    /// Optional human-readable label for a node. Returns `None` if the
    /// source has no name to attach (the default — many sources are
    /// label-less; e.g. raw `SliceSource` over freshly-minted
    /// `Uuid7`s has nothing meaningful to surface).
    ///
    /// Sources that know names — [`crate::sql_dump::SqlDumpSource`]
    /// knows table names, a future `MysqlMetaSource` will know
    /// `db.table` names — should override this method. The index
    /// builder calls it once per node during `build_from_source`;
    /// returned labels are persisted alongside structural data and
    /// surface back through `SwIndex::label_of` and
    /// `SwIndex::query_by_label`. Without labels the index works
    /// fine — it's just `Uuid7`-only at query time.
    ///
    /// # Default implementation
    ///
    /// Returns `None`. Backwards-compatible: every existing
    /// `GraphSource` impl that doesn't override gets "no labels"
    /// behavior, identical to v0.1.0.
    fn label_of(&self, node_id: crate::id::Uuid7) -> Option<String> {
        let _ = node_id;
        None
    }
}

/// A [`GraphSource`] backed by two slices held in memory.
///
/// `SliceSource` is the simplest possible source — it stores nothing,
/// owns nothing, just borrows two slices and exposes them as a
/// `GraphSource`. Used in unit tests, small examples, and any case
/// where the caller already has the full graph in memory and just
/// wants to feed it to the builder.
///
/// **Validation:** `SliceSource` does *not* check that every edge's
/// `source`/`target` ids appear in the node slice. That validation is
/// the index builder's responsibility — when the builder lands in a
/// later PR, it will surface dangling-edge errors as typed errors at
/// build time rather than at iteration time.
#[derive(Debug, Clone, Copy)]
pub struct SliceSource<'a> {
    nodes: &'a [Node],
    edges: &'a [Edge],
}

impl<'a> SliceSource<'a> {
    /// Wrap a pair of slices.
    ///
    /// The slices are not validated against each other; see the struct
    /// doc for why and where the validation lands instead.
    #[must_use]
    pub const fn new(nodes: &'a [Node], edges: &'a [Edge]) -> Self {
        Self { nodes, edges }
    }

    /// Number of nodes in the underlying slice (no iteration, no allocation).
    #[must_use]
    pub const fn node_len(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the underlying slice (no iteration, no allocation).
    #[must_use]
    pub const fn edge_len(&self) -> usize {
        self.edges.len()
    }
}

impl GraphSource for SliceSource<'_> {
    fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        // Cloning is cheap — Node is just a Uuid7 (16 bytes by value) plus
        // a NodeKind (a String). For graphs that fit in memory, this is
        // dominated by Leiden's downstream cost.
        self.nodes.iter().cloned()
    }

    fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.edges.iter().cloned()
    }

    fn node_count_hint(&self) -> Option<usize> {
        // For a slice we always know the exact count, so the hint is
        // tight rather than approximate.
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

    /// Tiny fixture: a parcel owned by an owner — enough structure to
    /// exercise iteration without being a real graph.
    fn make_demo() -> (Vec<Node>, Vec<Edge>) {
        let parcel = Node::fresh(NodeKind::new("parcel"));
        let owner = Node::fresh(NodeKind::new("owner"));
        let nodes = vec![parcel.clone(), owner.clone()];
        let edges = vec![Edge::fresh(owner.id, parcel.id, EdgeKind::new("owns"))];
        (nodes, edges)
    }

    #[test]
    fn empty_source_yields_nothing() {
        // Empty slices must iterate to empty and report Some(0) — not
        // None, since we genuinely know the count is zero.
        let src = SliceSource::new(&[], &[]);
        assert_eq!(src.nodes().count(), 0);
        assert_eq!(src.edges().count(), 0);
        assert_eq!(src.node_count_hint(), Some(0));
        assert_eq!(src.edge_count_hint(), Some(0));
    }

    #[test]
    fn yields_exactly_the_input() {
        // Round-trip through the trait: the items the iterator produces
        // must equal the items the slice contains, no transformation.
        let (nodes, edges) = make_demo();
        let src = SliceSource::new(&nodes, &edges);

        let collected_nodes: Vec<_> = src.nodes().collect();
        let collected_edges: Vec<_> = src.edges().collect();
        assert_eq!(collected_nodes, nodes);
        assert_eq!(collected_edges, edges);
    }

    #[test]
    fn iteration_is_repeatable() {
        // Contract clause (3) — calling nodes()/edges() twice must yield
        // the same sequence both times. The builder relies on this when
        // it iterates edges several times during a Leiden pass.
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
        // For SliceSource specifically the hint is exact, not approximate.
        let (nodes, edges) = make_demo();
        let src = SliceSource::new(&nodes, &edges);
        assert_eq!(src.node_count_hint(), Some(2));
        assert_eq!(src.edge_count_hint(), Some(1));
        assert_eq!(src.node_len(), 2);
        assert_eq!(src.edge_len(), 1);
    }

    /// Confirms that `GraphSource` works as a generic bound, the way the
    /// future `SwIndex::build_from_source(source: impl GraphSource)` will
    /// use it. If this fn ever stops compiling, the trait shape has
    /// regressed in a way that would break every downstream consumer.
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
