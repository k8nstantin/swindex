// clippy::doc_markdown fires on common acronyms and product names in
// prose-heavy doc comments (MySQL, NetworkX, GraphML, JSON, etc.).
// Wrapping every such token in backticks is noise; we silence the
// lint crate-wide and rely on review to catch genuinely-confusing prose.
#![allow(clippy::doc_markdown)]

//! `swindex` — hierarchical small-world property-graph index.
//!
//! A persistent, online, query-routing index for property graphs. Built from
//! Leiden community detection plus hub identification plus recursive
//! aggregation; achieves O(log N) typical query complexity on graphs that
//! exhibit emergent small-world structure.
//!
//! See [`DESIGN.md`](https://github.com/k8nstantin/swindex/blob/main/DESIGN.md)
//! in the repository root for the full design document. This is v0.0.5 —
//! core types, the [`GraphSource`] boundary, a GML loader, an in-memory
//! [`Graph`], and Louvain community detection. Hub detection, persistence,
//! and the query planner land in subsequent releases.
//!
//! # Module map
//!
//! * [`id`] — [`Uuid7`], the v7-only intrinsic identity used throughout.
//! * [`node`] — [`Node`] and [`Edge`] plus their typed kind labels.
//! * [`source`] — the [`GraphSource`] trait + the [`SliceSource`] reference
//!   implementation for in-memory tests.
//! * [`gml`] — [`gml::GmlSource`], a loader for academic GML files
//!   (Zachary, SNAP datasets, NetworkX exports). Implements [`GraphSource`].
//! * [`graph`] — [`Graph`], an in-memory undirected/weighted graph built
//!   from any [`GraphSource`]. The substrate the clustering algorithms
//!   actually walk.
//! * [`community`] — [`community::Partition`], [`community::modularity`],
//!   and Louvain (the first community-detection algorithm) — produces
//!   modularity ≥ 0.3 on Zachary karate. Leiden refinement lands next.

pub mod community;
pub mod gml;
pub mod graph;
pub mod id;
pub mod node;
pub mod source;

pub use community::{Partition, louvain, louvain_seeded, modularity};
pub use gml::{GmlError, GmlSource};
pub use graph::{Graph, GraphError};
pub use id::Uuid7;
pub use node::{Edge, EdgeId, EdgeKind, Node, NodeId, NodeKind};
pub use source::{GraphSource, SliceSource};

/// Returns the crate version as declared in `Cargo.toml`.
///
/// Useful both as a smoke test (a downstream consumer can call this to
/// prove their dependency on swindex compiles and links correctly) and
/// as a runtime identifier (logs, metrics, error reports).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_non_empty() {
        // Guard against `env!` returning an empty string if Cargo's
        // metadata is misconfigured — has actually happened in
        // workspace setups where the package isn't built first.
        assert!(!version().is_empty());
    }

    #[test]
    fn version_matches_cargo_pkg_version() {
        // The crate-level `version()` and the macro-evaluated env var
        // must agree. If this ever fails, something is rebuilding the
        // package without rebuilding callers.
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
