# jjn

Working repo for the substrate / swindex venture.

Source docs live here:

- [`research.md`](research.md) — venture thesis, market, moat.
- [`small-world-index.md`](small-world-index.md) — design doc for `swindex`, a standalone hierarchical small-world property-graph index in Rust.
- [`prototype.md`](prototype.md) — design doc for `substrate-rs`, the application layer built on top of `swindex`.
- [`deployment_plan.md`](deployment_plan.md) — concrete bootstrap plan to get `swindex` installed and working.

## Quickstart

```bash
git clone git@github.com:k8nstantin/jjn.git
cd jjn
cargo test
```

Or as a dependency in another Rust project:

```bash
cargo add swindex --git https://github.com/k8nstantin/jjn
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
