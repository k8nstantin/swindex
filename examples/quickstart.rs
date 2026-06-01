//! End-to-end quickstart — load a graph, build an index, run a query.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example quickstart
//! ```
//!
//! Expected output (on the bundled Zachary karate fixture):
//!
//! ```text
//! 34 nodes, 4 clusters, 4 regions, 4 hubs
//! N similar uuids (M clusters visited, K hub-graph hops)
//! ```
//!
//! This is the same snippet referenced from the project README. Keeping
//! it here (rather than only in the README) means `cargo build
//! --examples` verifies the snippet still compiles every PR.

use swindex::source::GraphSource;
use swindex::{EdgeKind, GmlSource, NodeKind, QueryKind, SwIndex};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open (or create) the index at a directory path. Fjall manages
    //    the on-disk layout inside it. Using a tempdir here so running
    //    the example repeatedly doesn't accumulate state.
    let dir = tempfile::TempDir::new()?;
    let mut idx = SwIndex::open(dir.path())?;

    // 2. Build from any GraphSource. Here we use Zachary's karate club
    //    from the repo's tests/fixtures directory; in production you'd
    //    write an adapter against your MySQL/Iceberg/etc rows.
    let src = GmlSource::from_path(
        "tests/fixtures/karate.gml",
        &NodeKind::new("member"),
        &EdgeKind::new("friendship"),
    )?;
    let build_stats = idx.build_from_source(&src)?;
    println!(
        "{} nodes, {} clusters, {} regions, {} hubs",
        build_stats.nodes, build_stats.clusters, build_stats.regions, build_stats.hubs,
    );

    // 3. Query the index. `Similar` walks the four-layer router:
    //    cluster lookup → cluster_members → hub-graph expansion →
    //    neighbor clusters' members. Truncated at `limit`.
    let seed = src.nodes().next().expect("graph has at least one node");

    // limit=25 is larger than the seed's cluster (~12 members on
    // Zachary), so this forces the hub-graph expansion path to fire
    // and visit other clusters via Layer 2.
    let result = idx.query(QueryKind::Similar {
        start: seed.id,
        limit: 25,
    })?;

    println!(
        "{} similar uuids ({} clusters visited, {} hub-graph hops)",
        result.uuids.len(),
        result.stats.clusters_visited,
        result.stats.hubs_visited,
    );

    // 4. The result is a list of `Uuid7`s — go fetch their row data
    //    from your primary store (MySQL / Iceberg / Postgres / etc).
    //
    // `dir` drops at end of scope; for a long-lived index you'd open
    // at a stable path and let SwIndex outlive this function.

    Ok(())
}
