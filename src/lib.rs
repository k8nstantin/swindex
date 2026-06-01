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
//! in the repository root for the full design document. This is v0.0.8 —
//! core types, the [`GraphSource`] boundary, a GML loader, an in-memory
//! [`Graph`], Louvain & Leiden community detection, and degree-based hub
//! detection. Approximate betweenness centrality, the hub graph,
//! persistence, and the query planner land in subsequent releases.
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
//!   from any [`GraphSource`]. The substrate the algorithms walk.
//! * [`community`] — [`community::Partition`], [`community::modularity`],
//!   Louvain and Leiden community detection. Produces Q ≈ 0.42 on
//!   Zachary karate with provably-connected communities.
//! * [`hub`] — [`hub::HubSet`], degree-based hub identification. The
//!   Layer-2 hub set that the query planner routes through.
//! * [`hub_graph`] — [`hub_graph::HubGraph`], the Layer-2 adjacency
//!   structure derived from a `Graph` plus a `HubSet`. Built by BFS
//!   up to a `k_hop` ball; edge weights = `1 / hop_distance`.
//! * [`region`] — [`region::RegionGraph`], the Layer-3 cluster→region
//!   mapping derived by running Leiden on the cluster super-graph
//!   (recursive Leiden, same trick Microsoft GraphRAG uses offline).
//! * [`index`] — [`index::SwIndex`], the persisted public face. Wraps a
//!   Fjall keyspace with six partitions holding the four-layer
//!   structural metadata. `build_from_source` runs the full Layer-0..3
//!   pipeline and commits atomically; close + reopen round-trips
//!   identical answers.

pub mod community;
pub mod gml;
pub mod graph;
pub mod hub;
pub mod hub_graph;
pub mod id;
pub mod index;
pub mod maintenance;
pub mod node;
pub mod quality;
pub mod region;
pub mod source;

pub use community::{
    Partition, leiden, leiden_seeded, louvain, louvain_seeded, modularity, regions_from_clusters,
};
pub use gml::{GmlError, GmlSource};
pub use graph::{Graph, GraphError};
pub use hub::HubSet;
pub use hub_graph::HubGraph;
pub use id::Uuid7;
pub use index::{BuildStats, QueryKind, QueryResult, QueryStats, SwIndex, SwIndexError, SwStats};
pub use maintenance::{
    ClusterDrift, DriftReport, MaintenanceAction, MaintenancePolicy, MaintenanceReport,
    NeverRebalance,
};
pub use node::{Edge, EdgeId, EdgeKind, Node, NodeId, NodeKind};
pub use quality::nmi;
pub use region::RegionGraph;
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
