//! `swindex` — hierarchical small-world property-graph index.
//!
//! A persistent, online, query-routing index for property graphs. Built from
//! Leiden community detection plus hub identification plus recursive
//! aggregation; achieves O(log N) typical query complexity on graphs that
//! exhibit emergent small-world structure.
//!
//! See `small-world-index.md` in the repository root for the full design
//! document. This is v0.0.3 — core types and the [`GraphSource`] boundary.
//! Index algorithms (Leiden, hubs, persistence, query) land in subsequent
//! releases.

pub mod id;
pub mod node;
pub mod source;

pub use id::Uuid7;
pub use node::{Edge, EdgeId, EdgeKind, Node, NodeId, NodeKind};
pub use source::{GraphSource, SliceSource};

/// Returns the crate version as declared in `Cargo.toml`.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }

    #[test]
    fn version_matches_cargo_pkg_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
