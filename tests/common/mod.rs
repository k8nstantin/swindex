// Shared helpers used across integration tests under `tests/`.
//
// Cargo treats `tests/common/mod.rs` as a shared module — NOT a test
// target — so we can `mod common;` it from any `tests/*.rs` file and
// use `common::generate_sbm(...)`. This keeps the SBM generator out of
// the public crate API where it has no business living.
//
// Anything added here should be a *test fixture builder*, not
// production code. Production primitives belong in `src/`.

#![allow(dead_code)] // each integration test only uses a subset

use swindex::{Edge, EdgeKind, Node, NodeKind, Partition};

// ===========================================================================
// Deterministic xorshift64 — identical to `community::shuffled_indices` and
// `benches/scaling.rs::xorshift64`. Replayable graph generation needs a
// pinned PRNG; pulling in `rand` for this would be overkill.
// ===========================================================================

/// Advance the xorshift64 state once and return the new word.
pub fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Map a u64 into the half-open unit interval `[0, 1)` using the top
/// 53 bits — same f64-precision-aware conversion as `benches/scaling.rs`.
#[allow(clippy::cast_precision_loss)]
pub fn unit_uniform(state: &mut u64) -> f64 {
    let r = xorshift64(state);
    ((r >> 11) as f64) / ((1_u64 << 53) as f64)
}

// ===========================================================================
// Stochastic Block Model (SBM) graph generator
// ===========================================================================

/// A graph with planted community structure, used to measure clustering
/// quality against a known ground truth.
pub struct PlantedGraph {
    /// Nodes, in deterministic order. `nodes[i]` corresponds to
    /// planted-community label `planted.community_of(i)`.
    pub nodes: Vec<Node>,
    /// Edges sampled from the SBM mixing parameters.
    pub edges: Vec<Edge>,
    /// Ground-truth community assignment over `nodes`.
    pub planted: Partition,
}

impl PlantedGraph {
    /// Convenience: how many edges the generator emitted. Useful for
    /// asserting density invariants in tests ("generator produced a
    /// non-trivial graph").
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

/// Generate a Stochastic Block Model graph with `n_nodes` nodes
/// distributed round-robin across `k` communities. An intra-community
/// edge is sampled with probability `p_in`, an inter-community edge
/// with probability `p_out`. Deterministic for a fixed `seed`.
///
/// Returns the nodes, edges, and the **planted partition** — the
/// ground truth `Leiden` is supposed to recover.
///
/// # Parameter intuition
///
/// * `p_in >> p_out` → easy recovery (high contrast).
/// * `p_in ≈ p_out` → near-percolation, recovery is hard or impossible.
/// * For roughly constant per-node degree across `n_nodes`, pick
///   `p_in ≈ target_avg_degree / (n_nodes / k - 1)`.
///
/// # Performance
///
/// `O(n_nodes^2)` — fine for the planted-partition correctness suite
/// where `n_nodes` stays in the hundreds. Scaling-bench callers should
/// keep using `benches/scaling.rs::sbm_graph` which is the same logic
/// but doesn't pay for returning the planted partition.
#[must_use]
pub fn generate_sbm(n_nodes: usize, k: usize, p_in: f64, p_out: f64, seed: u64) -> PlantedGraph {
    assert!(n_nodes > 0, "generate_sbm: n_nodes must be > 0");
    assert!(k > 0, "generate_sbm: k must be > 0");
    assert!(
        (0.0..=1.0).contains(&p_in),
        "generate_sbm: p_in out of range"
    );
    assert!(
        (0.0..=1.0).contains(&p_out),
        "generate_sbm: p_out out of range"
    );

    let mut state = if seed == 0 { 1 } else { seed };

    let nodes: Vec<Node> = (0..n_nodes)
        .map(|_| Node::fresh(NodeKind::new("v")))
        .collect();

    // Round-robin community assignment so cluster sizes differ by at most 1.
    let cluster_of: Vec<usize> = (0..n_nodes).map(|i| i % k).collect();

    let mut edges = Vec::new();
    for i in 0..n_nodes {
        for j in (i + 1)..n_nodes {
            let same_cluster = cluster_of[i] == cluster_of[j];
            let p = if same_cluster { p_in } else { p_out };
            if unit_uniform(&mut state) < p {
                edges.push(Edge::fresh(nodes[i].id, nodes[j].id, EdgeKind::new("e")));
            }
        }
    }

    PlantedGraph {
        nodes,
        edges,
        planted: Partition::new(cluster_of),
    }
}
