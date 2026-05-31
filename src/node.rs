//! The atomic units of the graph: typed nodes and typed directed edges.
//!
//! A [`Node`] is a stable identity ([`Uuid7`]) plus a [`NodeKind`] label that
//! tells the index which type-bucket the node belongs to. An [`Edge`] is a
//! signed reference from one node to another with a typed [`EdgeKind`]. Both
//! are pure data — no algorithms, no persistence, no I/O.
//!
//! Attribute payloads (the rich property bag a real fact carries) are not
//! modeled here yet. Layer those on in a later PR; the index only needs the
//! structural skeleton.

use crate::id::Uuid7;
use serde::{Deserialize, Serialize};

/// Stable identity of a node in the graph.
pub type NodeId = Uuid7;

/// Stable identity of an edge in the graph.
pub type EdgeId = Uuid7;

/// A typed label distinguishing one kind of node from another.
///
/// `NodeKind` is intentionally a thin newtype over `String` so applications
/// can use any vocabulary they like (`"parcel"`, `"deed"`, `"contractor"`,
/// `"hvac_unit"`, …) without swindex prescribing a closed enum.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeKind(String);

impl NodeKind {
    /// Construct a `NodeKind` from any string-like value.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// The kind as a string slice.
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
/// Same shape as [`NodeKind`] — a vocabulary the application owns. Examples:
/// `"owns"`, `"located_at"`, `"replaces"`, `"signed_by"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeKind(String);

impl EdgeKind {
    /// Construct an `EdgeKind` from any string-like value.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// The kind as a string slice.
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Node {
    /// Stable v7 identity.
    pub id: NodeId,
    /// Application-defined type label.
    pub kind: NodeKind,
}

impl Node {
    /// Construct a `Node` from an existing id and kind.
    #[must_use]
    pub fn new(id: NodeId, kind: NodeKind) -> Self {
        Self { id, kind }
    }

    /// Mint a fresh `Node` with a newly minted [`Uuid7`].
    #[must_use]
    pub fn fresh(kind: NodeKind) -> Self {
        Self::new(Uuid7::now(), kind)
    }
}

/// A directed edge from one node to another, with a type label.
///
/// The edge itself has its own stable id so it can be the subject of further
/// facts (provenance, signatures, supersession) in higher layers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    /// Stable v7 identity for this edge.
    pub id: EdgeId,
    /// The source node.
    pub source: NodeId,
    /// The target node.
    pub target: NodeId,
    /// Application-defined type label.
    pub kind: EdgeKind,
}

impl Edge {
    /// Construct an `Edge` from existing ids and a kind.
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
        let k = NodeKind::new("parcel");
        let json = serde_json::to_string(&k).unwrap();
        let back: NodeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(k, back);
        assert_eq!(json, "\"parcel\"");
    }

    #[test]
    fn edge_kind_as_str_matches_input() {
        let k = EdgeKind::new("owns");
        assert_eq!(k.as_str(), "owns");
        assert_eq!(k.as_ref(), "owns");
    }

    #[test]
    fn fresh_node_has_v7_id_and_given_kind() {
        let n = Node::fresh(NodeKind::new("parcel"));
        assert_eq!(n.kind.as_str(), "parcel");
        assert_eq!(n.id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn node_round_trip_preserves_value() {
        let n = Node::fresh(NodeKind::new("parcel"));
        let json = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn fresh_edge_links_given_endpoints() {
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
        let e = Edge::fresh(Uuid7::now(), Uuid7::now(), EdgeKind::new("owns"));
        let json = serde_json::to_string(&e).unwrap();
        let back: Edge = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }
}
