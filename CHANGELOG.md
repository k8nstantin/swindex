# Changelog

All notable changes to swindex are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `CHANGELOG.md`, `CONTRIBUTING.md`, and `examples/quickstart.rs` (operational hygiene per architectural review #44 §7).
- `docs/HISTORY.md` preserving the original bootstrap plan as a historical artifact (was at `deployment_plan.md` in the repo root).
- `BENCHMARKS.md` updated with the actual measured numbers from the v0.1.0 bench run and the correct SBM-parameter calibration.
- GitHub repo description and homepage URL.
- **`tracing` instrumentation on `SwIndex`** (review #44 §7.7). `open`, `build_from_source`, and `query` now emit `info`-level spans; `build_from_source` emits a debug-level sub-span per pipeline phase (`graph`, `leiden`, `regions`, `hubs`, `hub_graph`, `persist`) with structured fields (graph size, cluster count, hub count, query stats). No subscriber wired up; caller's choice. See the module-level doc for a quickstart with `tracing-subscriber`.
- **Format version byte on all variable-length `SwIndex` encodings** (issue #49, review #44 §3.3). `cluster_members`, `hub_neighbors`, and `cluster_meta` payloads now start with `FORMAT_V1 = 0x01`. Decoders refuse any other byte with the new `SwIndexError::UnsupportedFormat { found, context }` variant — distinct from `Corruption` so operator messaging can route upgrade-prompts separately from data-integrity alerts. Fixed-width partitions (`uuid_to_cluster`, `uuid_to_region`, `uuid_is_hub`) deliberately do **not** carry a version byte; they're trivially evolvable by widening the value type and re-detecting at open time. 4 new tests cover round-trip + future-version rejection + empty-payload rejection.
- **Quality module + concept-validation suite** (issue #41, Phase 0 of the v0.2 roadmap). New public `swindex::nmi(a, b)` — Normalized Mutual Information between two `Partition`s. New integration test target `tests/clustering_quality.rs` builds planted-partition SBM graphs at three difficulty levels and asserts (a) Leiden recovers the planted partition (NMI ≥ 0.95 / 0.85 / 0.70 for easy / medium / hard) and (b) `SwIndex::query` returns results from the seed's planted community at high precision (Similar avg-precision@20 ≥ 0.85; SameCluster precision ≥ 0.95; round-trip preserves relevance). This is the load-bearing concept validation: it proves the static index produces *relevant* query results — before any incremental-maintenance work is built on top.
- **Incremental-maintenance interface scaffolding** (issue #52, Phase 1 of the v0.2 roadmap; advances #27). New `swindex::maintenance` module with `MaintenancePolicy` trait, `NeverRebalance` impl, `MaintenanceAction` enum (`#[non_exhaustive]` for forward compat), `DriftReport`, and `ClusterDrift`. New `SwIndex::insert_node(&Node, &[Uuid7]) -> Result<u32>` appends a node by majority-vote of its seed neighbors (ties → lowest cluster id; no known seeds → new singleton). New `SwIndex::drift_report() -> Result<DriftReport>` reads per-cluster insert pressure. New `SwIndex::maintain(&P) -> Result<MaintenanceReport>` runs whatever a policy decides — `NeverRebalance` is a no-op. New on-disk partition `cluster_drift` (FORMAT_V1 + 8-byte generation + 4-byte delta_inserts = 13 B), initialized by `build_from_source` for every cluster. **Phase 2+ will swap in real rebalancing policies behind this same interface, no API changes.** 8 new tests cover majority-vote, tie-breaking, singleton fallback, duplicate-uuid rejection, drift round-trip, no-op maintain, and close/reopen persistence.
- **Label persistence + name-based query API** (issue #55). New `labels` and `label_to_uuid` Fjall partitions persist human-readable names alongside the structural data. New `GraphSource::label_of(uuid) -> Option<String>` trait method (default impl returns `None` — backwards compatible). `SqlDumpSource` overrides to return its qualified table name (`db.table`). New `SwIndex` methods: `label_of(uuid)`, `uuid_of_label(name)`, `query_by_label(kind)`. New types `QueryKindByLabel` and `QueryResultByLabel` mirror their uuid-typed counterparts but take/return strings. Unknown seed labels return empty results (not errors) — right ergonomics for "find me things like X." Result uuids without registered labels fall back to their uuid hex string so the caller always gets printable output. 6 new tests cover round-trip, reverse lookup, close/reopen persistence, priority-ordered results, unknown-seed-is-empty, and partial-label fallback.
- **`SqlDumpSource` — first real-world `GraphSource`** (issue #54). Parses `mysqldump --no-data` output (CREATE TABLE + FOREIGN KEY) into a `GraphSource` of table-nodes and FK-edges. Hand-rolled line-oriented parser (~470 LOC, no SQL-grammar dependency); covers backtick-quoted identifiers, multi-line CREATE TABLE, inline + standalone `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` syntax, `ON DELETE` / `ON UPDATE` clauses, MySQL block comments, self-referential FKs, dangling FKs (silently dropped with warning). Critically: tracks `USE \`db\`;` to **qualify table names across multi-database dumps** — the same `tbl_X` in `db_a` vs `db_b` is treated as two distinct nodes (validated on real Gryphon schema where 4 databases reuse table names). Tracks `DELIMITER //` boundaries to **skip stored-procedure / function / trigger bodies**, so CREATE TABLE statements inside routine bodies don't count as real tables. New `examples/schema_inspect.rs` (`cargo run --example schema_inspect -- <path>`) is the operator-facing diagnostic: parses a dump, builds the index, prints clusters labeled with table names, runs sample `Similar` queries. **All `*.sql` files are blocked at every depth in `.gitignore` — the parser code commits, schemas don't.** 11 unit tests including round-trip, dangling FK, self-reference, multi-DB qualification, and DELIMITER routine-skip.

### Changed
- `deployment_plan.md` (repo root) → `docs/HISTORY.md` — the bootstrap is done; the file was stale and contradicted what was actually built.
- `BENCHMARKS.md` methodology section corrected: documents the **sparse** SBM (`p_in = target_avg_degree / (cluster_size − 1)`) actually used by `benches/scaling.rs`, not the unused dense parameters.
- **`SwIndex` on-disk format is now v1.** Indexes built with v0.1.0 (no format-version byte) can no longer be read by the current `decode_*` paths — they'll fail with `Corruption(...)` because the leading byte is interpreted as a version byte and won't be `0x01` in the general case. v0.1.0 is pre-release; no migration code is provided. Rebuild from source.

### Dependencies
- Added `tracing = "0.1"` (production dep).
- Added `tracing-subscriber = "0.3"` with `env-filter` feature (dev-dep only, for the module-level doctest and any future span-emission tests).

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
- ~~Binary persistence format has no version byte — see review notes #44 §3.3.~~ **Fixed in Unreleased; format is now v1.**

[Unreleased]: https://github.com/k8nstantin/swindex/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/k8nstantin/swindex/releases/tag/v0.1.0
