//! The atomic units of the graph: typed nodes and typed directed edges.
//!
//! # Data model
//!
//! A swindex graph is made of two things:
//!
//! * **[`Node`]** — a stable identity ([`Uuid7`]) plus a [`NodeKind`] label
//!   that tells the index which type-bucket this node belongs to. The
//!   label is used by hub detection (some kinds are always-hub-eligible —
//!   e.g. registry nodes, type nodes, institutional anchors) and by the
//!   query planner (a "find all parcels in region X" query filters by
//!   kind before it does any expensive traversal).
//!
//! * **[`Edge`]** — a directed reference between two nodes with its own
//!   stable identity and a [`EdgeKind`] label. The edge has its own id
//!   (not derived from `(source, target, kind)`) because higher layers
//!   need to attach provenance, signatures, and supersession events to
//!   the *edge as a fact* — and those edge-level facts need a stable
//!   pointer.
//!
//! # What's deliberately not here
//!
//! Rich attribute payloads (the "property bag" most real-world facts
//! carry — an address, an area, a sale price, a timestamp, an owner
//! signature) are *not* in this struct. swindex is an index, not a
//! data store: the payload lives in your existing system (MySQL,
//! Iceberg, Parquet, whatever) and swindex stores only the structural
//! skeleton it needs to do clustering and hub-aware traversal. Keeping
//! `Node` and `Edge` skeletal is what lets the index footprint stay at
//! 2–5% of the underlying data size.
//!
//! A future PR may add an optional `attrs: Option<Arc<dyn Any>>`-style
//! escape hatch for callers who want to keep payload alongside the
//! index for convenience, but that's a separate decision.
//!
//! # Why open-world kind strings, not a closed enum
//!
//! `NodeKind` and `EdgeKind` are newtypes over `String`, not enums.
//! That's deliberate. swindex is application-agnostic; it does not know
//! whether your domain calls things `parcel` / `deed` / `owner` (real
//! estate) or `paper` / `author` / `cites` (citation graph) or
//! `endpoint` / `service` / `talks_to` (network topology). Forcing a
//! closed enum would either constrain the vocabulary or force every
//! application to pile its values into a generic `Other(String)`
//! variant, defeating the point.

use crate::id::Uuid7;
use serde::{Deserialize, Serialize};

/// Stable identity of a node in the graph.
///
/// Alias for [`Uuid7`]. The distinct name is for documentation — function
/// signatures that take a `NodeId` express intent more clearly than ones
/// that take a bare `Uuid7`.
pub type NodeId = Uuid7;

/// Stable identity of an edge in the graph.
///
/// Alias for [`Uuid7`]. Edges have their own identity (not derived from
/// the endpoint pair) so that edge-level facts — provenance, signatures,
/// supersession — can attach to a stable pointer.
pub type EdgeId = Uuid7;

/// A typed label distinguishing one kind of node from another.
///
/// `NodeKind` is intentionally a thin newtype over `String` so applications
/// can use any vocabulary they like (`"parcel"`, `"deed"`, `"contractor"`,
/// `"hvac_unit"`, …) without swindex prescribing a closed enum. See the
/// module doc for the rationale.
///
/// The string can be anything; swindex itself never interprets it, only
/// stores it and groups by it. Two `NodeKind`s with the same string are
/// equal (`Eq + Hash`), so the type works fine as a HashMap key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeKind(String);

impl NodeKind {
    /// Construct a `NodeKind` from any string-like value.
    ///
    /// Accepts both `&str` (allocating a new `String`) and `String`
    /// (moving the existing allocation) via the `Into<String>` bound.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// The kind as a string slice — cheap accessor with no allocation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for NodeKind {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A typed label distinguishing one kind of edge from another.
///
/// Same shape as [`NodeKind`] — a vocabulary the application owns.
/// Examples: `"owns"`, `"located_at"`, `"replaces"`, `"signed_by"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeKind(String);

impl EdgeKind {
    /// Construct an `EdgeKind` from any string-like value.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// The kind as a string slice — cheap accessor with no allocation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EdgeKind {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A node in the graph: identity plus type label.
///
/// Two ways to construct one: [`Node::new`] when you already have an id
/// (e.g. ingesting from another store that already assigned UUIDv7s), and
/// [`Node::fresh`] when you want swindex to mint one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Node {
    /// Stable v7 identity. Set once at construction, never mutated.
    pub id: NodeId,
    /// Application-defined type label. Drives hub detection and query
    /// planning — see the module doc for the semantics.
    pub kind: NodeKind,
}

impl Node {
    /// Construct a `Node` from an existing id and kind.
    ///
    /// Use this when ingesting from a source that already assigned a
    /// `Uuid7` (e.g. another swindex instance, a `state_ledger` row).
    /// Use [`Node::fresh`] when you need a brand-new identity.
    #[must_use]
    pub fn new(id: NodeId, kind: NodeKind) -> Self {
        Self { id, kind }
    }

    /// Mint a fresh `Node` with a newly minted [`Uuid7`].
    ///
    /// Equivalent to `Node::new(Uuid7::now(), kind)`. The most common
    /// constructor during initial ingestion.
    #[must_use]
    pub fn fresh(kind: NodeKind) -> Self {
        Self::new(Uuid7::now(), kind)
    }
}

/// A directed edge from one node to another, with a type label.
///
/// The edge has its own stable id so it can be the subject of further
/// facts in higher layers — provenance, signatures, supersession events
/// all attach to *the edge*, not to the endpoint pair. Without an edge
/// id, "this edge was signed by X" has no stable referent if the same
/// `(source, target, kind)` is later re-asserted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    /// Stable v7 identity for this edge, distinct from the endpoints.
    pub id: EdgeId,
    /// The source endpoint.
    pub source: NodeId,
    /// The target endpoint.
    pub target: NodeId,
    /// Application-defined type label.
    pub kind: EdgeKind,
}

impl Edge {
    /// Construct an `Edge` from existing ids and a kind.
    ///
    /// Use when re-hydrating an edge whose id was previously assigned;
    /// use [`Edge::fresh`] when you're creating one for the first time.
    #[must_use]
    pub fn new(id: EdgeId, source: NodeId, target: NodeId, kind: EdgeKind) -> Self {
        Self {
            id,
            source,
            target,
            kind,
        }
    }

    /// Mint a fresh `Edge` with a newly minted edge [`Uuid7`].
    ///
    /// The endpoint ids must already exist (or be about to be created in
    /// the same atomic ingestion); swindex does not invent endpoints.
    #[must_use]
    pub fn fresh(source: NodeId, target: NodeId, kind: EdgeKind) -> Self {
        Self::new(Uuid7::now(), source, target, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::{Edge, EdgeKind, Node, NodeKind};
    use crate::id::Uuid7;

    #[test]
    fn node_kind_round_trip_preserves_value() {
        // Pin the JSON shape: serde(transparent) means a NodeKind is just
        // its inner string, not `{"NodeKind": "..."}`. This is what every
        // serialized swindex fact will look like on the wire.
        let k = NodeKind::new("parcel");
        let json = serde_json::to_string(&k).unwrap();
        let back: NodeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(k, back);
        assert_eq!(json, "\"parcel\"");
    }

    #[test]
    fn edge_kind_as_str_matches_input() {
        // Both as_str (the inherent method) and as_ref (the trait) must
        // return the exact original string with no normalization.
        let k = EdgeKind::new("owns");
        assert_eq!(k.as_str(), "owns");
        assert_eq!(k.as_ref(), "owns");
    }

    #[test]
    fn fresh_node_has_v7_id_and_given_kind() {
        // fresh() should mint a real v7 id (not nil, not v4) and preserve
        // the kind argument verbatim.
        let n = Node::fresh(NodeKind::new("parcel"));
        assert_eq!(n.kind.as_str(), "parcel");
        assert_eq!(n.id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn node_round_trip_preserves_value() {
        // Full Node JSON round-trip — id stringifies, kind is transparent,
        // and the whole thing parses back equal.
        let n = Node::fresh(NodeKind::new("parcel"));
        let json = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn fresh_edge_links_given_endpoints() {
        // The edge's own id must be distinct from either endpoint id;
        // otherwise higher layers can't unambiguously refer to "the edge"
        // versus "the endpoint."
        let a = Uuid7::now();
        let b = Uuid7::now();
        let e = Edge::fresh(a, b, EdgeKind::new("owns"));
        assert_eq!(e.source, a);
        assert_eq!(e.target, b);
        assert_eq!(e.kind.as_str(), "owns");
        assert_ne!(e.id, a, "edge id must be distinct from endpoint ids");
        assert_ne!(e.id, b, "edge id must be distinct from endpoint ids");
    }

    #[test]
    fn edge_round_trip_preserves_value() {
        // Full Edge JSON round-trip including source/target ids and kind.
        let e = Edge::fresh(Uuid7::now(), Uuid7::now(), EdgeKind::new("owns"));
        let json = serde_json::to_string(&e).unwrap();
        let back: Edge = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }
}
