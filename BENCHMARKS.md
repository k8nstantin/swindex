# Benchmarks

Empirical measurements of swindex's build, query, and storage behavior across graph sizes. Run with `cargo bench --bench scaling` from the repo root.

All numbers in this document come from running the bench suite at a single point in time on a single machine — they should be taken as **order-of-magnitude** numbers, not as gospel. The point is to verify the complexity claims (`O(N log N)` build, `O(log N)` typical query), not to publish absolute throughput records.

## Methodology

- **Graph generator:** Stochastic Block Model (SBM). For each N we generate `K=10` clusters, with intra-cluster edge probability `p_in=0.05` and inter-cluster `p_out=0.0005`. Deterministic given seed `42`.
- **Sizes:** N = 1,000, 10,000, 50,000. The 10⁶ and 10⁷ scales from the design doc are deferred to a future "long-run" benchmark — those individual builds take multiple minutes and don't fit a normal `cargo bench` budget.
- **Tooling:** [criterion 0.5](https://crates.io/crates/criterion) — runs each benchmark for at least 8 seconds (or until enough samples are collected) and reports a confidence interval per point.
- **Machine:** local developer laptop. Re-running on a different machine will produce different absolute numbers — but the *scaling slopes* should match.

## Groups

### `graph_build`

Time to construct an in-memory `Graph` from a `SliceSource`. This is just the adjacency-list build and the per-node degree precomputation — no clustering, no persistence.

Expected complexity: `O(N + E)`. For an SBM graph with our parameters, `E` grows roughly linearly with `N²` for in-cluster edges, but in practice `p_in` is small enough that `E ≪ N²`.

_(Numbers will be filled in once the bench completes.)_

### `swindex_build`

Full `SwIndex::build_from_source` pipeline:
1. `Graph::from_source` (Layer 0)
2. `leiden(&graph)` — multi-level community detection (Layer 1)
3. `RegionGraph::build` — recursive Leiden on the cluster super-graph (Layer 3)
4. `HubSet::from_top_fraction(graph, 0.10)` — top-10% by degree (Layer 2)
5. `HubGraph::build(graph, hubs, 3)` — BFS up to depth 3 (Layer 2)
6. Atomic Fjall batch write — 6 partitions

Expected complexity: dominated by Leiden, which is `O(E · log N)` with our serial implementation. The other layers are `O(E)` or `O(H × k_hop · degree)`.

_(Numbers will be filled in once the bench completes.)_

### `query_similar`

`SwIndex::query(QueryKind::Similar { limit: 25 })` against a pre-built index.

The seed node sits in a cluster of size ~N/K (so 100, 1000, 5000 at our three sizes). The query's behavior:
- At N=1,000: cluster has ~100 members, returns first 25 — no hub-graph expansion needed
- At N=10,000: cluster has ~1,000 members, returns first 25 — no expansion
- At N=50,000: cluster has ~5,000 members, returns first 25 — no expansion

Because the seed's own cluster is always large enough to satisfy `limit=25` at these scales, this benchmark mostly measures the cluster-members fetch, not the hub-graph walk. To exercise the hub-graph path at larger scales we'd need either a much larger limit or a smaller cluster — covered separately by `query_similar_zachary` (where the cluster has 12 members and a limit of 25 forces expansion).

Expected complexity: dominated by the cluster-members fetch — `O(cluster_size + log N)` per query.

_(Numbers will be filled in once the bench completes.)_

### `query_similar_zachary`

A sanity check on the real Zachary karate fixture. Pre-built index, `Similar { limit: 25 }` queries.

This is the test that does exercise the hub-graph expansion path (cluster has 12 members, limit 25 forces it to walk to neighboring clusters via hubs). Time per query should be a few microseconds.

_(Numbers will be filled in once the bench completes.)_

## How to reproduce

```bash
git clone git@github.com:k8nstantin/swindex.git
cd swindex
cargo bench --bench scaling
```

Criterion writes detailed HTML reports under `target/criterion/` — open `target/criterion/report/index.html` for per-benchmark plots, histograms, and confidence intervals.

## What we are NOT measuring (yet)

- **`O(N log N)` claim at N ≥ 10⁶.** The current sweep stops at N=50k. Running at 10⁶ takes minutes per sample; needs a separate "long" bench group that's opt-in.
- **Insert-throughput / `O(log N) incremental update`.** Requires the incremental maintenance path from issue #27 (Ada-IVF). Today `build_from_source` is rebuild-only.
- **Time-travel query latency.** Requires the Parquet history tables from issue #29.
- **Comparison against scan-based alternatives** (Neo4j, Postgres recursive CTE, etc). Honest comparisons require running the same workload against both stores; deferred to a future "competitive" bench suite.

## Followups

- Issue #41 — LFR planted-partition correctness validation (NMI ≥ 0.9 at μ ≤ 0.5)
- Issue #42 — SNAP dataset compatibility check (web-Google, cit-Patents, roadNet-CA)
- Long-run benchmarks at 10⁶ / 10⁷ (will land as a separate `benches/long_scaling.rs` once the v0.2 milestone is otherwise complete)
