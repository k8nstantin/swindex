# Changelog

All notable changes to swindex are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `CHANGELOG.md`, `CONTRIBUTING.md`, and `examples/quickstart.rs` (operational hygiene per architectural review #44 §7).
- `docs/HISTORY.md` preserving the original bootstrap plan as a historical artifact (was at `deployment_plan.md` in the repo root).
- `BENCHMARKS.md` updated with the actual measured numbers from the v0.1.0 bench run and the correct SBM-parameter calibration.
- GitHub repo description and homepage URL.

### Changed
- `deployment_plan.md` (repo root) → `docs/HISTORY.md` — the bootstrap is done; the file was stale and contradicted what was actually built.
- `BENCHMARKS.md` methodology section corrected: documents the **sparse** SBM (`p_in = target_avg_degree / (cluster_size − 1)`) actually used by `benches/scaling.rs`, not the unused dense parameters.

---

## [0.1.0] — 2026-05-31

First working release. All four architecture layers shipped + persistence + structured query API. 80 tests passing.

### Added
- **Layer 0: full fact graph.** `Graph`, `GraphSource` trait, `SliceSource`, `GmlSource` (GML loader for academic datasets — Zachary, SNAP, NetworkX exports).
- **Layer 1: cluster graph.** `louvain` and `leiden` (Traag-Waltman-van-Eck 2019), `modularity` (Newman-Girvan), `Partition`. Multi-level with aggregation and refinement. Zachary modularity Q = 0.4188 with provably internally-connected communities.
- **Layer 2: hub graph.** `HubSet` (degree-based) + `HubGraph` (k-hop BFS adjacency with weight = 1/hop_distance, symmetric by construction).
- **Layer 3: region graph.** `RegionGraph` via recursive Leiden over the cluster super-graph (Microsoft GraphRAG pattern, applied online).
- **Persistence:** `SwIndex` wrapping Fjall LSM with 6 partitions (uuid→cluster, uuid→region, uuid→is_hub, hub_neighbors, cluster_members, cluster_meta). Atomic build, close + reopen round-trips identical answers.
- **Query API:** `QueryKind::{SameCluster, Similar}`. `Similar` walks Layer 1 → Layer 2 → Layer 1 for "find me things related to X."
- **Identity:** `Uuid7` newtype with v7 invariant enforcement; deliberately no infallible `From<Uuid>` impl.
- **Marketing site:** [k8nstantin.github.io/swindex](https://k8nstantin.github.io/swindex/), deployed from `/site` via GitHub Pages workflow.
- **License:** BSL 1.1 with hosted-index-as-a-service carve-out; auto-converts to Apache 2.0 four years after release.
- **CI:** `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace` on every PR. Branch protection on `main`.

### Known limitations (tracked in issues)
- Approximate betweenness centrality not implemented — degree-only hub detection ([#23](https://github.com/k8nstantin/swindex/issues/23)).
- Incremental updates not implemented — `build_from_source` is rebuild-only and leaves stale entries from prior builds ([#27](https://github.com/k8nstantin/swindex/issues/27)).
- Time-travel queries not implemented ([#29](https://github.com/k8nstantin/swindex/issues/29)).
- Bench sweep stops at N=50k; the design's full sweep to 10⁷ is deferred to a long-run bench group ([#28](https://github.com/k8nstantin/swindex/issues/28) tracking; also #41 for LFR, #42 for SNAP).
- Region routing is structurally present (cluster→region mapping) but not yet wired into the query planner — see review notes #44 §3.1.
- Binary persistence format has no version byte — see review notes #44 §3.3.

[Unreleased]: https://github.com/k8nstantin/swindex/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/k8nstantin/swindex/releases/tag/v0.1.0
