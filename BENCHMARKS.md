# Benchmarks

Empirical measurements of swindex's build and query behavior across graph sizes. Run with `cargo bench --bench scaling` from the repo root.

All numbers in this document come from running the bench suite at a single point in time on a single machine. They are **order-of-magnitude** numbers, not absolute throughput records. The point is to verify the complexity claims (`O(N log N)` build, `O(log N)` typical query), and where the data doesn't quite show that, to document what it does show.

> _Last refreshed: 2026-06-01, local developer laptop (macOS, Apple silicon)._

---

## Methodology

- **Graph generator:** Stochastic Block Model (SBM) with **constant target average degree** — `p_in = target_avg_degree / (cluster_size − 1)` and `p_out ≈ 0.5 / N`. This gives total edges `E ≈ (target_avg_degree / 2) · N`, linear in N. Without this calibration, fixed-`p_in` would make E grow as N² and "build time vs N" would just measure edge-count growth. Constants used by [`benches/scaling.rs`](benches/scaling.rs):
  ```
  K_CLUSTERS         = 10
  TARGET_AVG_DEGREE  = 10.0
  SEED               = 42
  ```
- **Sizes:** N = 1,000 / 10,000 / 50,000. The 10⁶ and 10⁷ scales from the design doc are deferred to a future "long-run" benchmark — those individual builds take minutes per iteration and don't fit a normal `cargo bench` budget.
- **Tooling:** [`criterion 0.5`](https://crates.io/crates/criterion). Each bench runs for at least 8 s (15 s for `swindex_build`, which is slower) and reports a confidence interval per point.
- **Determinism:** every benchmark uses seed `42` everywhere. Reproducible across runs on the same machine.

---

## Results

### `graph_build`

Time to construct an in-memory `Graph` from a `SliceSource`. No clustering, no persistence — just adjacency-list build and per-node degree precomputation.

| N | Time | Edges (≈) | Time / edge |
|---|---:|---:|---:|
| 1,000 | **650 µs** | ~5,000 | 130 ns |
| 10,000 | **9.3 ms** | ~50,000 | 186 ns |
| 50,000 | **1.5 s** | ~250,000 | 6,000 ns |

**Observation.** The N=50k row jumps an order of magnitude per unit work. The cause is `SliceSource::nodes()` / `edges()` yielding **owned** values — at 50k nodes + 250k edges that's ~300K heap allocations per build (every `NodeKind` / `EdgeKind` `String` clones), and the allocator dominates the actual graph-construction work.

This is a known cost. The fix is a trait change (a parallel `for_each_node` / `for_each_edge` API yielding `&Node` / `&Edge`) — filed as a follow-up. Not blocking v0.2; flagged in BENCHMARKS for honesty.

### `swindex_build`

Full pipeline: `Graph::from_source` → `leiden` → `RegionGraph` → `HubSet` → `HubGraph` → atomic Fjall batch write with `PersistMode::SyncAll`.

| N | Time | Notes |
|---|---:|---|
| 1,000 | **390 ms** | fsync (~200 ms on APFS) dominates; algorithm work is small |
| 10,000 | **4.93 s** | algorithm work overtakes fsync |
| 50,000 | **3.93 s** | noisier — only 10 samples per size + Leiden's iteration count varies by topology |

**Observation.** The N=50k point being slightly *faster* than N=10k is sample noise from `sample_size(10)` plus Leiden converging in a different number of iterations on different topologies. We'd need more samples (or `measurement_time` × 10) to pin a tight bound. The order of magnitude — **seconds, not minutes**, at 50k with persistence + fsync included — is what matters for the headline claim.

### `query_similar`

`SwIndex::query(QueryKind::Similar { limit: 25 })` against a pre-built index. Seed is the first-minted Uuid7 (lowest internal index).

| N | Time |
|---|---:|
| 1,000 | **1.34 µs** |
| 10,000 | **3.49 µs** |
| 50,000 | **12.5 µs** |

**This is the headline result.** Even at N=50,000 nodes, a `Similar` query completes in **~12 microseconds**.

**Scaling shape:** `12.5 µs / 1.34 µs ≈ 9.3` for `50k / 1k = 50×` more nodes. Sub-linear in N, but not strictly `O(log N)`. The actual scaling is roughly `O(cluster_size)` because the bottleneck is the Fjall read of `cluster_members` (which contains N/K ≈ N/10 uuids at 16 bytes each). Decoding + copying that array dominates.

**What this group does and doesn't exercise.** With `K_CLUSTERS = 10`, every benchmarked size has clusters of ~N/10 members (100 / 1,000 / 5,000) — far above `limit = 25`. Leiden recovers the planted SBM blocks exactly, so every query here is answered entirely from the seed's own cluster: `clusters_visited = 1`, `hubs_visited = 0`, verified empirically via `QueryStats`. **These numbers measure the cluster-lookup + members-fetch path only.** The hub-graph expansion path is exercised only by `query_similar_zachary` below (whose clusters of 5–12 members are smaller than the limit); its latency at scale is unmeasured — that needs a bench with cluster_size < limit at large N.

For tighter `O(log N)` query latency we'd need either:
- Smaller clusters (higher K), so per-query work scales as N/K, not N.
- A streaming cluster-members iterator that stops at the `limit`.

Both are easy follow-ups. **Microseconds at 50k stands on its own as a result, but it neither demonstrates nor approximates the design's `O(log N)` routing claim — that remains a design target, unmeasured.**

### `query_similar_zachary`

Sanity check on the real Zachary karate fixture (34 nodes, 4 clusters).

| | Time |
|---|---:|
| Zachary, limit=25 | **2.27 µs** |

At limit=25 the result must span multiple clusters (each Zachary cluster has 5–12 members), so this exercises the hub-graph expansion path. Confirms the bench machinery agrees with the integration-test data.

---

## What the data does and doesn't show

### Demonstrated
- ✅ **Query latency is microseconds**, not milliseconds, even at N=50k. This is the swindex value proposition for traversal-heavy workloads.
- ✅ **Within-cluster query latency scales sub-linearly with N** (9× for 50× more nodes) — but note these queries never leave the seed cluster (see scaling-shape note above), so this measures the members-fetch path, not hub routing.
- ✅ **Full pipeline build at 50k completes in seconds with persistence included.**
- ✅ **Atomic build** (verified separately in `index::tests::round_trip_via_close_and_reopen`).

### Not demonstrated (yet)
- ❌ Strict `O(N log N)` build at large N — sweep stops at 50k.
- ❌ Strict `O(log N)` query — actual scaling is `O(cluster_size)`, bounded by Fjall fetch.
- ❌ Hub-graph expansion latency at scale — the SBM sweep never fires it (`hubs_visited = 0` at every size); only the 34-node Zachary bench exercises that path.
- ❌ Insert-throughput / `O(log N)` incremental update — requires Ada-IVF maintenance from issue [#27](https://github.com/k8nstantin/swindex/issues/27).
- ❌ Time-travel query latency — requires Parquet history tables from issue [#29](https://github.com/k8nstantin/swindex/issues/29).
- ❌ Comparison against scan-based alternatives (Neo4j, Postgres recursive CTE) — competitive bench setup is a separate workstream.
- ❌ The `graph_build` 50k number (1.5 s) is dominated by `SliceSource` heap allocations, not by `Graph::from_source` itself. Honest reporting; the underlying construction is much faster.

---

## How to reproduce

```bash
git clone git@github.com:k8nstantin/swindex.git
cd swindex
cargo bench --bench scaling
```

Criterion writes detailed HTML reports under `target/criterion/` — open `target/criterion/report/index.html` for per-benchmark plots, histograms, and confidence intervals.

---

## Follow-ups

- [#41](https://github.com/k8nstantin/swindex/issues/41) — LFR planted-partition correctness validation (NMI ≥ 0.9 at μ ≤ 0.5)
- [#42](https://github.com/k8nstantin/swindex/issues/42) — SNAP dataset compatibility check (web-Google, cit-Patents, roadNet-CA)
- (TBD) — `GraphSource` trait change to avoid `SliceSource` clone overhead
- (TBD) — Long-run benchmark group at N = 10⁶ / 10⁷
- (TBD) — Smaller default K + streaming `cluster_members` for tighter `O(log N)` query latency
