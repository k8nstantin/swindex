//! `EdgeListSource` — loader for SNAP-style edge-list files
//! (issue #42).
//!
//! The SNAP datasets (<https://snap.stanford.edu/data/>) ship as plain
//! text: one `src dst` pair per line, whitespace-separated (tabs in
//! practice), with `#` comment lines. Node ids are arbitrary sparse
//! integers. Several "undirected" SNAP files list each edge in both
//! directions (ca-AstroPh does); swindex's [`crate::Graph`] is
//! undirected, so this loader **dedupes** by unordered pair — the
//! resulting graph has unit weight per distinct undirected edge, which
//! is what Leiden's modularity and Brandes' path counting both expect.
//!
//! Self-loops are kept (the modularity convention handles them; the
//! betweenness module drops them itself). Original numeric ids are
//! preserved as labels via [`crate::GraphSource::label_of`], so query
//! results can be mapped back to the published node ids.

use crate::id::Uuid7;
use crate::node::{Edge, EdgeKind, Node, NodeKind};
use crate::source::GraphSource;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

/// Errors from loading an edge-list file.
#[derive(Debug)]
pub enum EdgeListError {
    /// Underlying file read failed.
    Io(std::io::Error),
    /// A non-comment line didn't parse as two integer ids.
    Parse {
        /// 1-based line number in the input.
        line_no: usize,
        /// The offending line, for the error message.
        line: String,
    },
}

impl fmt::Display for EdgeListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdgeListError::Io(e) => write!(f, "edge-list io error: {e}"),
            EdgeListError::Parse { line_no, line } => {
                write!(f, "edge-list parse error at line {line_no}: {line:?}")
            }
        }
    }
}

impl std::error::Error for EdgeListError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EdgeListError::Io(e) => Some(e),
            EdgeListError::Parse { .. } => None,
        }
    }
}

impl From<std::io::Error> for EdgeListError {
    fn from(e: std::io::Error) -> Self {
        EdgeListError::Io(e)
    }
}

/// A fully-materialized edge-list graph source. See the module doc
/// for format details.
pub struct EdgeListSource {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    /// `Uuid7` → original numeric id rendered as a string; serves
    /// [`GraphSource::label_of`].
    labels: BTreeMap<Uuid7, String>,
}

impl EdgeListSource {
    /// Load an edge-list file from disk.
    ///
    /// # Errors
    ///
    /// [`EdgeListError::Io`] if the file can't be read;
    /// [`EdgeListError::Parse`] (with a 1-based line number) for any
    /// non-comment line that isn't two whitespace-separated integers.
    pub fn from_path(
        path: impl AsRef<Path>,
        node_kind: &NodeKind,
        edge_kind: &EdgeKind,
    ) -> Result<Self, EdgeListError> {
        let text = fs::read_to_string(path)?;
        Self::from_str(&text, node_kind, edge_kind)
    }

    /// Parse edge-list text. `node_kind` / `edge_kind` are stamped
    /// onto every node and edge.
    ///
    /// # Errors
    ///
    /// [`EdgeListError::Parse`] for any malformed non-comment line.
    #[allow(clippy::should_implement_trait)] // same naming convention as GmlSource::from_str
    pub fn from_str(
        text: &str,
        node_kind: &NodeKind,
        edge_kind: &EdgeKind,
    ) -> Result<Self, EdgeListError> {
        // First pass: collect raw pairs and the distinct id set.
        // BTreeSet/BTreeMap keep everything deterministic regardless
        // of input order.
        let mut raw_ids: BTreeSet<u64> = BTreeSet::new();
        let mut pairs: BTreeSet<(u64, u64)> = BTreeSet::new();
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut fields = trimmed.split_whitespace();
            let (Some(a), Some(b), None) = (fields.next(), fields.next(), fields.next()) else {
                return Err(EdgeListError::Parse {
                    line_no: idx + 1,
                    line: line.to_string(),
                });
            };
            let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>()) else {
                return Err(EdgeListError::Parse {
                    line_no: idx + 1,
                    line: line.to_string(),
                });
            };
            raw_ids.insert(a);
            raw_ids.insert(b);
            // Unordered dedupe: files that list both directions of an
            // undirected edge collapse to one unit-weight edge.
            pairs.insert((a.min(b), a.max(b)));
        }

        // Second pass: mint nodes in ascending original-id order
        // (deterministic), then edges in ascending pair order.
        let mut by_raw: BTreeMap<u64, Uuid7> = BTreeMap::new();
        let mut nodes = Vec::with_capacity(raw_ids.len());
        let mut labels = BTreeMap::new();
        for raw in raw_ids {
            let node = Node::fresh(node_kind.clone());
            by_raw.insert(raw, node.id);
            labels.insert(node.id, raw.to_string());
            nodes.push(node);
        }
        let edges = pairs
            .into_iter()
            .map(|(a, b)| Edge::fresh(by_raw[&a], by_raw[&b], edge_kind.clone()))
            .collect();

        Ok(Self {
            nodes,
            edges,
            labels,
        })
    }

    /// Distinct node count parsed from the file.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Distinct undirected edge count after deduplication.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

impl GraphSource for EdgeListSource {
    fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        self.nodes.iter().cloned()
    }

    fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.edges.iter().cloned()
    }

    fn label_of(&self, node_id: Uuid7) -> Option<String> {
        self.labels.get(&node_id).cloned()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // hand-computed exact degree values
mod tests {
    use super::{EdgeListError, EdgeListSource};
    use crate::graph::Graph;
    use crate::node::{EdgeKind, NodeKind};
    use crate::source::GraphSource;

    fn kinds() -> (NodeKind, EdgeKind) {
        (NodeKind::new("n"), EdgeKind::new("e"))
    }

    /// Comments, blank lines, tabs, and duplicate both-direction
    /// listings (the SNAP "undirected" convention) must all collapse
    /// to a clean undirected simple graph.
    #[test]
    fn parses_snap_conventions() {
        let (nk, ek) = kinds();
        let text = "# Directed graph (each unordered pair listed twice)\n\
                    # FromNodeId\tToNodeId\n\
                    10\t20\n\
                    20\t10\n\
                    \n\
                    20 30\n\
                    30\t10\n";
        let src = EdgeListSource::from_str(text, &nk, &ek).unwrap();
        assert_eq!(src.node_count(), 3);
        // (10,20) listed twice -> one edge; plus (20,30), (10,30).
        assert_eq!(src.edge_count(), 3);

        let g = Graph::from_source(&src).unwrap();
        assert_eq!(g.node_count(), 3);
        // Triangle: every node has degree 2.
        for i in 0..3 {
            assert_eq!(g.degree(i), 2.0);
        }
    }

    /// Original numeric ids survive as labels — that's how results
    /// map back to the published dataset ids.
    #[test]
    fn labels_preserve_original_ids() {
        let (nk, ek) = kinds();
        let src = EdgeListSource::from_str("7 99\n", &nk, &ek).unwrap();
        let labels: Vec<String> = src
            .nodes()
            .map(|n| src.label_of(n.id).expect("every node is labeled"))
            .collect();
        assert_eq!(labels, vec!["7".to_string(), "99".to_string()]);
    }

    /// Self-loops are kept — Graph tracks them and downstream
    /// algorithms apply their own conventions.
    #[test]
    fn self_loops_are_kept() {
        let (nk, ek) = kinds();
        let src = EdgeListSource::from_str("5 5\n5 6\n", &nk, &ek).unwrap();
        assert_eq!(src.edge_count(), 2);
        let g = Graph::from_source(&src).unwrap();
        assert!(g.self_loop(0) > 0.0);
    }

    /// Malformed lines fail loudly with the 1-based line number —
    /// silent skipping would corrupt the graph invisibly.
    #[test]
    fn malformed_line_reports_line_number() {
        let (nk, ek) = kinds();
        // `unwrap_err` would require `EdgeListSource: Debug`; match the
        // Err arm directly instead of deriving Debug on a big struct.
        let Err(err) = EdgeListSource::from_str("1 2\nnot numbers\n", &nk, &ek) else {
            panic!("expected a parse error on a non-numeric line");
        };
        match err {
            EdgeListError::Parse { line_no, .. } => assert_eq!(line_no, 2),
            other @ EdgeListError::Io(_) => panic!("expected Parse, got {other}"),
        }
        let Err(err) = EdgeListSource::from_str("1 2 3\n", &nk, &ek) else {
            panic!("expected a parse error on a three-field line");
        };
        assert!(matches!(err, EdgeListError::Parse { line_no: 1, .. }));
    }

    /// Same text → same structure (node order, edge order). The
    /// loader is BTree-backed, so this guards against someone
    /// swapping in a HashMap later.
    #[test]
    fn parse_is_deterministic() {
        let (nk, ek) = kinds();
        let text = "3 1\n2 3\n1 2\n";
        let a = EdgeListSource::from_str(text, &nk, &ek).unwrap();
        let b = EdgeListSource::from_str(text, &nk, &ek).unwrap();
        let labels_a: Vec<_> = a.nodes().map(|n| a.label_of(n.id).unwrap()).collect();
        let labels_b: Vec<_> = b.nodes().map(|n| b.label_of(n.id).unwrap()).collect();
        assert_eq!(labels_a, labels_b);
        assert_eq!(labels_a, vec!["1", "2", "3"]);
    }
}
