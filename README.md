# swindex

A hierarchical small-world property-graph index in Rust. Builds and maintains a layered Leiden-community + hub-graph structure over arbitrary property graphs, so multi-hop traversal queries scale to billions of nodes with O(log N) typical latency — what HNSW does for vectors, but for arbitrary structured property data.

**swindex is an index, not a database.** Your data stays in whatever store already holds it — MySQL, Postgres, Iceberg, Parquet, Arrow, an HTTP API, whatever — and swindex sits alongside it as a sidecar that narrows multi-hop graph queries down to a small bounded set of candidates. Then your application goes back to its primary store to fetch the actual rows.

## Architecture in 30 seconds

```
                    application query
                          │
                          ▼
              ┌──────────────────────────┐
              │   swindex (Rust crate)   │
              │   ─────────────────────  │
              │   • cluster of node X?   │   ← O(log N) hub-aware traversal
              │   • members of cluster?  │
              │   • multi-hop pattern?   │
              │                          │   returns a small list of UUIDv7 ids
              └────────────┬─────────────┘
                           │
              ┌────────────▼─────────────┐
              │  YOUR store (Iceberg /   │   ← columnar scan filtered by uuid7
              │  MySQL / Postgres / ...) │     returns the actual row payloads
              └──────────────────────────┘
```

swindex stores **only structural metadata** (cluster assignments, hub graph, cluster→region mapping, optionally history for time-travel queries). On disk that's typically 2–5% of the size of the underlying data. Your row payloads stay where they live.

## Quickstart

Clone and run the tests:

```bash
git clone git@github.com:k8nstantin/swindex.git
cd swindex
cargo test
```

Use as a dependency:

```bash
cargo add swindex --git https://github.com/k8nstantin/swindex
```

```rust
use swindex::{Edge, EdgeKind, Node, NodeKind, SliceSource, Uuid7};

fn main() {
    let parcel = Node::fresh(NodeKind::new("parcel"));
    let owner  = Node::fresh(NodeKind::new("owner"));
    let owns   = Edge::fresh(owner.id, parcel.id, EdgeKind::new("owns"));

    let nodes = [parcel, owner];
    let edges = [owns];
    let _src = SliceSource::new(&nodes, &edges);

    println!("swindex {}", swindex::version());
}
```

The full design — Leiden community detection, hub-graph navigation, four-layer query routing, Ada-IVF-style incremental maintenance, persistent storage layout — is documented in [`DESIGN.md`](DESIGN.md). The bootstrap plan from scratch is in [`deployment_plan.md`](deployment_plan.md).

This is v0.0.2 — early days. Current shipped surface: core identity (`Uuid7`), node/edge types, the `GraphSource` trait, and a `SliceSource` reference implementation. The Leiden / hub / storage / query / maintenance layers land in subsequent releases.

## License

[BSL 1.1](LICENSE) — free for embedding inside applications or services whose primary value is something other than the indexing functionality itself. Hosted-index-as-a-service offerings require a commercial license. Auto-converts to Apache 2.0 four years after each release.

## Workflow

All work happens on feature branches via PR. The trunk (`main`) is always releasable: every commit must pass `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace`. Enforced by branch protection + CI on every PR.
