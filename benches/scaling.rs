// Same rationale as in the main crate: prose comments mention product
// names and acronyms (Leiden, Fjall, SBM, etc.) that don't need
// backticking. Silence the lint for this bench target only.
#![allow(clippy::doc_markdown, clippy::items_after_statements)]

//! Scaling benchmarks: build and query latency vs graph size N.
//!
//! Run with `cargo bench --bench scaling`. Each benchmark group uses
//! `criterion`'s statistical analysis to get stable numbers despite
//! background noise.
//!
//! # What we measure
//!
//! * **`graph_build`** — `Graph::from_source` only. Just the in-memory
//!   adjacency construction, no clustering or persistence. Validates
//!   the cheap path scales linearly in N+E.
//! * **`swindex_build`** — full `SwIndex::build_from_source`: Graph +
//!   Leiden + RegionGraph + HubSet + HubGraph + write to Fjall. This
//!   is the headline build-cost number; the design claim is `O(N log N)`
//!   amortized.
//! * **`query_similar`** — `SwIndex::query(QueryKind::Similar { limit: 25 })`
//!   on a pre-built index. NOTE: with K_CLUSTERS = 10, every size's
//!   clusters (~N/10 members) far exceed limit = 25, so these queries
//!   are answered entirely from the seed's cluster (hubs_visited = 0)
//!   — this group measures the cluster-lookup + members-fetch path,
//!   NOT hub-graph expansion. See BENCHMARKS.md for the implications
//!   against the `O(log N)` design claim.
//!
//! # Sizes
//!
//! Three orders of magnitude — N = 1,000 / 10,000 / 50,000. The
//! design doc's full sweep goes to 10⁷ but those runs take minutes
//! per iteration and we don't have CI budget for that yet; capture
//! them in a follow-up that runs on demand. The three sizes here
//! are enough to fit a line and detect order-of-magnitude
//! regressions.
//!
//! # Graph generator
//!
//! Stochastic Block Model (SBM): K clusters of roughly equal size,
//! intra-cluster edge probability `p_in`, inter-cluster `p_out`.
//! Deterministic given a seed; produces graphs with planted
//! community structure (so Leiden has something coherent to find)
//! that scales smoothly with N.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::time::Duration;

use swindex::{Edge, EdgeKind, GmlSource, Graph, Node, NodeKind, QueryKind, SliceSource, SwIndex};

// =========================================================================
// SBM graph generator (deterministic, seeded)
// =========================================================================

/// A simple seeded xorshift64 RNG — same algorithm we use in
/// `community::shuffled_indices`. Sufficient for benchmark graph
/// generation; not cryptographic.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Generate an SBM graph with `n_nodes` nodes partitioned into `k`
/// clusters. Intra-cluster edges sampled with probability `p_in`,
/// inter-cluster with `p_out`. Deterministic for a given `seed`.
///
/// For honest scaling benchmarks the caller should choose `p_in`
/// such that **expected per-node degree is constant** across N
/// (e.g. `p_in = target_avg_degree / (cluster_size - 1)`). Otherwise
/// E grows as N² and "build time vs N" reflects edge count scaling
/// rather than swindex's per-edge work.
fn sbm_graph(n_nodes: usize, k: usize, p_in: f64, p_out: f64, seed: u64) -> (Vec<Node>, Vec<Edge>) {
    let mut state = if seed == 0 { 1 } else { seed };

    let nodes: Vec<Node> = (0..n_nodes)
        .map(|_| Node::fresh(NodeKind::new("v")))
        .collect();

    // Assign each node to a cluster (round-robin so cluster sizes
    // differ by at most 1).
    let cluster_of: Vec<usize> = (0..n_nodes).map(|i| i % k).collect();

    let mut edges = Vec::new();
    for i in 0..n_nodes {
        for j in (i + 1)..n_nodes {
            let same_cluster = cluster_of[i] == cluster_of[j];
            let p = if same_cluster { p_in } else { p_out };
            let r = xorshift64(&mut state);
            // Convert u64 to f64 in [0, 1) via top 53 bits.
            #[allow(clippy::cast_precision_loss)]
            let normalised = ((r >> 11) as f64) / ((1_u64 << 53) as f64);
            if normalised < p {
                edges.push(Edge::fresh(nodes[i].id, nodes[j].id, EdgeKind::new("e")));
            }
        }
    }

    (nodes, edges)
}

// =========================================================================
// Benchmark groups
// =========================================================================

const SIZES: &[usize] = &[1_000, 10_000, 50_000];
const K_CLUSTERS: usize = 10;
const TARGET_AVG_DEGREE: f64 = 10.0;
const SEED: u64 = 42;

/// Compute SBM parameters that give a constant target average
/// per-node degree across N. With `K` clusters of ~N/K nodes each
/// and `TARGET_AVG_DEGREE` average degree:
///
/// * `p_in = target_avg_degree / (cluster_size - 1)` — within-cluster
///   sparseness scales with cluster_size so each node has roughly
///   the target degree from intra-cluster edges alone.
/// * `p_out = small` — chosen low enough that intra-cluster edges
///   still dominate.
///
/// Result: total edges E ≈ N · TARGET_AVG_DEGREE / 2, linear in N.
/// This is what makes "build time vs N" a fair comparison.
#[allow(clippy::cast_precision_loss)]
fn sparse_sbm_params(n_nodes: usize) -> (f64, f64) {
    let cluster_size = (n_nodes / K_CLUSTERS).max(2) as f64;
    let p_in = (TARGET_AVG_DEGREE / (cluster_size - 1.0)).min(1.0);
    let p_out = 0.5 / (n_nodes as f64); // ~0.5 inter-cluster edges per node
    (p_in, p_out)
}

fn gen_graph(n: usize) -> (Vec<Node>, Vec<Edge>) {
    let (p_in, p_out) = sparse_sbm_params(n);
    sbm_graph(n, K_CLUSTERS, p_in, p_out, SEED)
}

/// `graph_build` — measures only the in-memory `Graph::from_source` step.
/// Sets the lower bound on the full build pipeline.
fn bench_graph_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_build");
    group.measurement_time(Duration::from_secs(8));
    for &n in SIZES {
        let (nodes, edges) = gen_graph(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let src = SliceSource::new(&nodes, &edges);
                let _g = Graph::from_source(&src).unwrap();
            });
        });
    }
    group.finish();
}

/// `swindex_build` — the full `SwIndex::build_from_source` pipeline,
/// including Leiden, hub detection, region detection, and Fjall write.
/// This is the headline build-cost number against the O(N log N) claim.
fn bench_swindex_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("swindex_build");
    group.measurement_time(Duration::from_secs(15));
    group.sample_size(10); // builds are expensive; 10 samples is enough
    for &n in SIZES {
        let (nodes, edges) = gen_graph(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter_with_setup(
                || {
                    // Fresh tempdir per iteration so each build starts
                    // from an empty keyspace.
                    let dir = tempfile::TempDir::new().expect("tmpdir");
                    let src = SliceSource::new(&nodes, &edges);
                    (dir, src)
                },
                |(dir, src)| {
                    let mut idx = SwIndex::open(dir.path()).expect("open");
                    let _stats = idx.build_from_source(&src).expect("build");
                    // dir drops here -> Fjall keyspace closes
                },
            );
        });
    }
    group.finish();
}

/// `query_similar` — pre-build a SwIndex once per size, then time the
/// `Similar` query. With K=10 clusters of ~N/10 nodes each, N/K is
/// 100/1,000/5,000 at the benchmarked sizes — always ≥ limit=25, so
/// the query never leaves the seed's cluster and the hub-graph
/// expansion path never fires here. (It fires on Zachary below,
/// whose clusters of 5–12 members are smaller than the limit.)
fn bench_query_similar(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_similar");
    group.measurement_time(Duration::from_secs(8));
    for &n in SIZES {
        let (nodes, edges) = gen_graph(n);
        // Build once outside the timed loop. Each query iteration
        // hits the warm Fjall keyspace.
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let mut idx = SwIndex::open(dir.path()).expect("open");
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).expect("build");
        let seed_uuid = nodes[0].id;

        group.throughput(Throughput::Elements(1)); // one query per iteration
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let _r = idx
                    .query(QueryKind::Similar {
                        start: seed_uuid,
                        limit: 25,
                    })
                    .expect("query");
            });
        });
        // dir drops at end of this scope — keyspace closes after the
        // last sample.
        drop(idx);
        drop(dir);
    }
    group.finish();
}

/// `query_similar_zachary` — sanity check on the real Zachary
/// fixture, to make sure the benchmark machinery agrees with the
/// integration-test numbers.
fn bench_query_similar_zachary(c: &mut Criterion) {
    let src = GmlSource::from_path(
        "tests/fixtures/karate.gml",
        &NodeKind::new("m"),
        &EdgeKind::new("f"),
    )
    .expect("load karate");
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let mut idx = SwIndex::open(dir.path()).expect("open");
    idx.build_from_source(&src).expect("build");

    use swindex::source::GraphSource;
    let seed = src.nodes().next().expect("at least one node");
    let mut group = c.benchmark_group("query_similar_zachary");
    group.bench_function("limit_25", |b| {
        b.iter(|| {
            let _r = idx
                .query(QueryKind::Similar {
                    start: seed.id,
                    limit: 25,
                })
                .expect("query");
        });
    });
    group.finish();
    drop(idx);
    drop(dir);
}

criterion_group!(
    benches,
    bench_graph_build,
    bench_swindex_build,
    bench_query_similar,
    bench_query_similar_zachary
);
criterion_main!(benches);
