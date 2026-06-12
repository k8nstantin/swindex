//! Corridor-metrics harness over the baseline strategy (issue #71).
//!
//! This target is the measurement rig the Gate-1 experiment (issue
//! #72) extends with the hub-navigation arm. It samples seeded
//! cross-cluster `(from, to)` pairs on graphs the index's *own*
//! Leiden partition sees (not the planted truth — corridors route on
//! what the index actually built), runs the cluster-adjacency
//! baseline corridor, judges every corridor against the BFS oracle,
//! and asserts the aggregate properties the baseline must hold:
//!
//! * **connected rate == 1.0** — a theorem for Leiden clusters plus
//!   super-adjacency, so any failure is a routing bug, not noise;
//! * **optimal rate above a regression floor** — deterministic given
//!   the fixed seeds, measured well above the floors asserted here;
//! * **corridors are small** — mean member fraction bounded away
//!   from 1.0, otherwise a corridor is just "fetch the whole graph".
//!
//! Gate 1's question — are *hub* corridors meaningfully smaller than
//! these baseline corridors at comparable recall? — is answered by
//! comparing against exactly these numbers.

mod common;

use common::{generate_sbm, sample_cross_cluster_pairs};
use swindex::{
    Graph, Partition, SliceSource, cluster_adjacency, cluster_corridor, evaluate_corridor, leiden,
};

/// Aggregate corridor quality over a pair sample.
#[derive(Debug, PartialEq)]
struct Aggregate {
    pairs: usize,
    connected: usize,
    optimal: usize,
    /// Mean fraction of the graph's nodes inside the corridor —
    /// the "size" cost the application pays per query.
    mean_member_fraction: f64,
}

/// Run the cluster-adjacency baseline over every sampled pair.
/// Globally-disconnected pairs are skipped (the corridor question is
/// moot); pairs whose clusters are disconnected in the super-graph
/// count as not-connected (the strategy failed to produce a corridor).
fn run_baseline(graph: &Graph, partition: &Partition, pairs: &[(usize, usize)]) -> Aggregate {
    let adjacency = cluster_adjacency(graph, partition);
    let n = graph.node_count();
    let mut agg = Aggregate {
        pairs: 0,
        connected: 0,
        optimal: 0,
        mean_member_fraction: 0.0,
    };
    let mut fraction_sum = 0.0_f64;
    for &(from, to) in pairs {
        let cf = partition.community_of(from);
        let ct = partition.community_of(to);
        let Some(corridor) = cluster_corridor(&adjacency, cf, ct) else {
            // Super-graph disconnection: only count it against the
            // strategy if the pair is globally reachable.
            if swindex::bfs_distance(graph, from, to).is_some() {
                agg.pairs += 1;
            }
            continue;
        };
        let Some(report) = evaluate_corridor(graph, partition, &corridor, from, to) else {
            continue; // globally unreachable — skip
        };
        agg.pairs += 1;
        if report.connected() {
            agg.connected += 1;
        }
        if report.optimal() {
            agg.optimal += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            fraction_sum += report.member_count as f64 / n as f64;
        }
    }
    if agg.pairs > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            agg.mean_member_fraction = fraction_sum / agg.pairs as f64;
        }
    }
    agg
}

/// Build a graph from an SBM preset and partition it the way the
/// index would — with Leiden, not with the planted labels.
fn sbm_with_leiden(n: usize, k: usize, p_in: f64, p_out: f64, seed: u64) -> (Graph, Partition) {
    let pg = generate_sbm(n, k, p_in, p_out, seed);
    let src = SliceSource::new(&pg.nodes, &pg.edges);
    let g = Graph::from_source(&src).unwrap();
    let p = leiden(&g);
    (g, p)
}

/// Easy SBM (the clustering-quality "easy" preset): the baseline must
/// connect every pair, stay near-optimal, and keep corridors well
/// under half the graph.
#[test]
fn sbm_easy_baseline_quality() {
    let (g, p) = sbm_with_leiden(400, 8, 0.40, 0.01, 0x00C0_FFEE);
    let pairs = sample_cross_cluster_pairs(g.node_count(), &p, 100, 0xFEED_BEEF);
    assert!(pairs.len() >= 90, "sampler starved: {} pairs", pairs.len());

    let agg = run_baseline(&g, &p, &pairs);
    assert_eq!(
        agg.connected, agg.pairs,
        "baseline corridor disconnected a pair: {agg:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let optimal_rate = agg.optimal as f64 / agg.pairs as f64;
    assert!(
        optimal_rate >= 0.80,
        "optimal rate regressed: {agg:?} (rate {optimal_rate:.3})"
    );
    assert!(
        agg.mean_member_fraction < 0.55,
        "corridors degenerating toward whole-graph fetches: {agg:?}"
    );
}

/// Medium SBM (the clustering-quality "medium" preset): weaker
/// community contrast, same structural guarantees.
#[test]
fn sbm_medium_baseline_quality() {
    let (g, p) = sbm_with_leiden(300, 6, 0.25, 0.025, 0x00C0_FFEE);
    let pairs = sample_cross_cluster_pairs(g.node_count(), &p, 100, 0xFEED_BEEF);
    assert!(pairs.len() >= 90, "sampler starved: {} pairs", pairs.len());

    let agg = run_baseline(&g, &p, &pairs);
    assert_eq!(
        agg.connected, agg.pairs,
        "baseline corridor disconnected a pair: {agg:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let optimal_rate = agg.optimal as f64 / agg.pairs as f64;
    assert!(
        optimal_rate >= 0.80,
        "optimal rate regressed: {agg:?} (rate {optimal_rate:.3})"
    );
    assert!(
        agg.mean_member_fraction < 0.75,
        "corridors degenerating toward whole-graph fetches: {agg:?}"
    );
}

/// The harness itself must be replayable: identical seeds produce
/// identical pair samples and identical aggregates, or Gate 1's
/// arm-vs-arm comparison means nothing.
#[test]
fn harness_is_deterministic() {
    let (g, p) = sbm_with_leiden(300, 6, 0.25, 0.025, 0x00C0_FFEE);
    let pairs_a = sample_cross_cluster_pairs(g.node_count(), &p, 60, 42);
    let pairs_b = sample_cross_cluster_pairs(g.node_count(), &p, 60, 42);
    assert_eq!(pairs_a, pairs_b);

    let agg_a = run_baseline(&g, &p, &pairs_a);
    let agg_b = run_baseline(&g, &p, &pairs_b);
    assert_eq!(agg_a, agg_b);

    // A different seed draws a different sample — guards against the
    // sampler silently ignoring its seed.
    let pairs_c = sample_cross_cluster_pairs(g.node_count(), &p, 60, 4242);
    assert_ne!(pairs_a, pairs_c);
}
