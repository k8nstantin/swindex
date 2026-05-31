# swindex

A hierarchical small-world property-graph index in Rust. Builds and maintains a layered Leiden-community + hub-graph structure over arbitrary property graphs, so multi-hop traversal queries scale to billions of nodes with O(log N) typical latency — what HNSW does for vectors, but for arbitrary structured property data.

## Design docs

- [`small-world-index.md`](small-world-index.md) — the swindex design doc (this is the library).
- [`deployment_plan.md`](deployment_plan.md) — concrete bootstrap plan to get swindex installed and working end-to-end.
- [`research.md`](research.md) — venture thesis context for the broader system swindex is part of.
- [`prototype.md`](prototype.md) — design doc for `substrate-rs`, an application layer that consumes swindex (separate project).

## Quickstart

Clone and run the tests:

```bash
git clone git@github.com:k8nstantin/swindex.git
cd swindex
cargo test
```

Use as a dependency in another Rust project:

```bash
cargo add swindex --git https://github.com/k8nstantin/swindex
```

```rust
use swindex::version;

fn main() {
    println!("swindex {}", version());
}
```

v0.0.1 ships the package skeleton only — algorithms (Leiden, hubs, persistence, query) land in subsequent PRs per the deployment plan.

## Workflow

All work happens on feature branches via PR. The trunk (`main`) is always releasable: every commit must pass `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace`. Enforced by CI on every PR.
