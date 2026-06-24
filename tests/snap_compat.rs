//! SNAP dataset compatibility checks (issue #42).
//!
//! These tests are `#[ignore]`d: the datasets are too large for the
//! repo and for CI. Run on demand after downloading into `data/snap/`
//! (the `data/` tree is gitignored):
//!
//! ```bash
//! mkdir -p data/snap && cd data/snap
//! curl -LO https://snap.stanford.edu/data/ca-AstroPh.txt.gz   && gunzip ca-AstroPh.txt.gz
//! curl -LO https://snap.stanford.edu/data/web-Google.txt.gz   && gunzip web-Google.txt.gz
//! curl -LO https://snap.stanford.edu/data/roadNet-CA.txt.gz   && gunzip roadNet-CA.txt.gz
//! curl -LO https://snap.stanford.edu/data/cit-Patents.txt.gz  && gunzip cit-Patents.txt.gz
//! cd ../.. && cargo test --release --test snap_compat -- --ignored --nocapture
//! ```
//!
//! Each check is the issue's spec verbatim: load → `Graph` → `leiden`
//! → assert a sane cluster count and modularity > 0.2 (real-world
//! graphs with community structure score far higher; 0.2 guards
//! against degenerate all-singletons / one-blob partitions, not
//! against quality drift).

use swindex::{EdgeKind, EdgeListSource, Graph, NodeKind, leiden, modularity};

fn check_dataset(file: &str, min_nodes: usize) {
    let path = format!("data/snap/{file}");
    let src = EdgeListSource::from_path(
        &path,
        &NodeKind::new("snap-node"),
        &EdgeKind::new("snap-edge"),
    )
    .unwrap_or_else(|e| {
        panic!(
            "could not load {path}: {e}\n\
             download it first — see this file's module doc for the curl commands"
        )
    });
    println!(
        "{file}: {} nodes, {} undirected edges",
        src.node_count(),
        src.edge_count()
    );
    assert!(
        src.node_count() >= min_nodes,
        "{file}: parsed suspiciously few nodes ({}) — truncated download?",
        src.node_count()
    );

    let g = Graph::from_source(&src).expect("edge list is self-consistent");
    let p = leiden(&g);
    let q = modularity(&g, &p);
    println!(
        "{file}: {} clusters, modularity {q:.4}",
        p.community_count()
    );

    // Sanity bounds from the issue spec: the partition is neither
    // all-singletons nor one giant blob, and modularity clears 0.2.
    assert!(q > 0.2, "{file}: modularity {q:.4} <= 0.2");
    assert!(
        p.community_count() > 1 && p.community_count() < g.node_count(),
        "{file}: degenerate partition ({} clusters over {} nodes)",
        p.community_count(),
        g.node_count()
    );
}

/// ca-AstroPh — 18,772 nodes, ~198K listed edges. Small enough to be
/// the Gate-1 experiment's first real graph; here it just has to
/// load and cluster sanely.
#[test]
#[ignore = "requires data/snap/ca-AstroPh.txt — see module doc"]
fn snap_ca_astroph() {
    check_dataset("ca-AstroPh.txt", 15_000);
}

/// web-Google — 875K nodes, 5.1M edges. The first scale test for the
/// single-threaded in-memory build; wall-clock here is Gate-4
/// evidence either way.
#[test]
#[ignore = "requires data/snap/web-Google.txt — see module doc"]
fn snap_web_google() {
    check_dataset("web-Google.txt", 800_000);
}

/// roadNet-CA — 2M nodes, 5.5M edges, near-planar (no hubs to speak
/// of — an adversarial topology for the hub-highway concept).
#[test]
#[ignore = "requires data/snap/roadNet-CA.txt — see module doc"]
fn snap_roadnet_ca() {
    check_dataset("roadNet-CA.txt", 1_900_000);
}

/// cit-Patents — 3.8M nodes, 16.5M edges. The largest of the set;
/// expected to stress the in-memory build hardest.
#[test]
#[ignore = "requires data/snap/cit-Patents.txt — see module doc"]
fn snap_cit_patents() {
    check_dataset("cit-Patents.txt", 3_500_000);
}
