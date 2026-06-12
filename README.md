# swindex

A hierarchical small-world property-graph index in Rust. Builds and persists a layered Leiden-community + hub-graph structure over arbitrary property graphs, and answers "what's related to X" queries in microseconds against your existing store. The design target — hub-routed traversal that scales the way HNSW does for vectors — is documented in [`DESIGN.md`](DESIGN.md); what's measured today is microsecond queries at 50k-node scale ([`BENCHMARKS.md`](BENCHMARKS.md)).

**swindex is an index, not a database.** Your data stays in whatever store already holds it — MySQL, Postgres, Iceberg, Parquet, Arrow, an HTTP API, whatever — and swindex sits alongside it as a sidecar that narrows multi-hop graph queries down to a small bounded set of candidates. Then your application goes back to its primary store to fetch the actual rows.

**Status:** v0.1.0 released — [release notes](https://github.com/k8nstantin/swindex/releases/tag/v0.1.0). All four architecture layers are built and persisted; queries currently route through two of them — cluster lookup plus one-hop hub expansion. Region routing and multi-hop hub navigation are tracked in the [v0.2 milestone](https://github.com/k8nstantin/swindex/issues). 142 tests passing.

## Architecture in 30 seconds

```
                    application query
                          │
                          ▼
              ┌──────────────────────────┐
              │   swindex (Rust crate)   │
              │   ─────────────────────  │
              │   • cluster of node X?   │   ← microsecond cluster lookup
              │   • members of cluster?  │
              │   • nodes similar to X?  │
              │                          │   returns a small list of UUIDv7 ids
              └────────────┬─────────────┘
                           │
              ┌────────────▼─────────────┐
              │  YOUR store (Iceberg /   │   ← columnar scan filtered by uuid7
              │  MySQL / Postgres / ...) │     returns the actual row payloads
              └──────────────────────────┘
```

swindex stores **only structural metadata** (cluster assignments, hub graph, cluster→region mapping, human-readable labels) — backed by a Fjall LSM keyspace with ten partitions. On disk that's typically 2–5% of the size of the underlying data. Your row payloads stay where they live.

### The four layers

| Layer | Structure | Purpose | Status |
|---|---|---|---|
| 3 | Region graph | "Which region(s) does this query touch?" Recursive Leiden over clusters. | Built + persisted; **not yet consulted at query time** (v0.2). |
| 2 | Hub graph | "Highway" — connected by k-hop BFS edges. Long-range navigation. | Built + persisted; queries do a **one-hop expansion** from one entry hub. Default selection is degree ∪ betweenness, 10% per criterion (design target 0.1–1%; tunable via `SwConfig`). |
| 1 | Cluster graph | Leiden-detected communities. Mathematically guaranteed well-connected (Leiden 2019). | Fully wired: every query starts here. |
| 0 | Full fact graph | Ground truth nodes + edges. | Lives in **your** store; swindex persists only the structural metadata above. |

## Quickstart

Clone and run the tests:

```bash
git clone git@github.com:k8nstantin/swindex.git
cd swindex
cargo test
# test result: ok. 133 passed; 0 failed   (plus 7 integration + 2 doc tests)
```

Use as a dependency:

```bash
cargo add swindex --git https://github.com/k8nstantin/swindex --tag v0.1.0
```

### A working end-to-end example

```rust
use swindex::{EdgeKind, GmlSource, NodeKind, QueryKind, SwIndex};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open (or create) the index at a directory path. Fjall manages
    //    the on-disk layout inside it.
    let mut idx = SwIndex::open("./data/swindex")?;

    // 2. Build from any GraphSource. Here we use Zachary's karate
    //    club from the repo's tests/fixtures directory; in production
    //    you'd write an adapter against your MySQL/Iceberg/etc rows.
    let src = GmlSource::from_path(
        "tests/fixtures/karate.gml",
        &NodeKind::new("member"),
        &EdgeKind::new("friendship"),
    )?;
    let build_stats = idx.build_from_source(&src)?;
    println!(
        "{} nodes, {} clusters, {} regions, {} hubs",
        build_stats.nodes, build_stats.clusters,
        build_stats.regions, build_stats.hubs,
    );

    // 3. Query the index. `Similar` walks the implemented route:
    //    cluster lookup → cluster_members → one-hop hub-graph expansion
    //    → neighbor clusters' members. Truncated at `limit`.
    use swindex::source::GraphSource;
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

    Ok(())
}
```

The full design — Leiden community detection, hub-graph navigation, four-layer query routing, Ada-IVF-style incremental maintenance, persistent storage layout — is documented in [`DESIGN.md`](DESIGN.md). Bench methodology and current numbers in [`BENCHMARKS.md`](BENCHMARKS.md). Release history in [`CHANGELOG.md`](CHANGELOG.md). Contribution guide in [`CONTRIBUTING.md`](CONTRIBUTING.md). The original bootstrap plan (archival) is at [`docs/HISTORY.md`](docs/HISTORY.md).

## What's shipped in v0.1.0

| Module | What |
|---|---|
| `id` | `Uuid7` — v7-invariant-enforcing newtype over `uuid::Uuid` |
| `node` | `Node`, `Edge`, `NodeKind`, `EdgeKind` |
| `source` | `GraphSource` trait + `SliceSource` reference impl |
| `gml` | `GmlSource` — loader for academic GML files (Zachary, SNAP, NetworkX) |
| `graph` | Internal undirected/weighted `Graph` |
| `community` | `louvain`, `leiden`, `modularity`, `Partition`, `regions_from_clusters` |
| `hub` | `HubSet` — degree-based hub identification |
| `hub_graph` | `HubGraph` — Layer-2 adjacency among hubs |
| `region` | `RegionGraph` — Layer-3 cluster → region mapping |
| `index` | `SwIndex` — persisted public face: open / build / query / stats |

What's **not** in v0.1.0 (see [open issues](https://github.com/k8nstantin/swindex/issues) for the v0.2 roadmap):
- ~~Approximate betweenness centrality (Brandes' algorithm) — issue #23~~ Shipped on `main` (unreleased): `betweenness` module + `HubSet::from_centrality`, and `build_from_source` now defaults to degree ∪ betweenness hub selection via `SwConfig` (#68).
- Region routing + multi-hop hub navigation in the query planner (queries currently use cluster lookup + one-hop hub expansion)
- Incremental Ada-IVF maintenance — issue #27
- Benchmark suite at 10⁴–10⁷ scale — issue #28
- Time-travel (`query_as_of`) — issue #29
- Open-ended `Pattern` query abstraction — query layer extension

## License

[BSL 1.1](LICENSE) — free for embedding inside applications or services whose primary value is something other than the indexing functionality itself. Hosted-index-as-a-service offerings require a commercial license. Auto-converts to Apache 2.0 four years after each release.

## Workflow

All work happens on feature branches via PR. Every PR is backed by a tracking issue with acceptance criteria. The trunk (`main`) is always releasable: every commit must pass `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace`. Enforced by branch protection + CI on every PR.
