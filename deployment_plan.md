# swindex — Get It Installed and Working

Cut the bullshit. Here's how to bootstrap the index library and prove it works.

## Step 0: Make sure Rust is installed

```bash
rustc --version
# If "command not found":
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup default stable
rustc --version   # should print 1.80+ or similar
```

## Step 1: Create the workspace

```bash
mkdir -p ~/gryphon/swindex && cd ~/gryphon/swindex
cargo init --lib --name swindex
git init && git add -A && git commit -m "scaffold"
```

`Cargo.toml` becomes a workspace once we add sub-crates, but start with the simplest single-crate layout. Don't split until you actually need to.

## Step 2: Add the dependencies

Edit `Cargo.toml` — add only what's needed to start. Resist adding everything in the design doc on day one.

```toml
[package]
name = "swindex"
version = "0.0.1"
edition = "2021"

[dependencies]
petgraph = "0.6"
uuid = { version = "1", features = ["v7"] }
rayon = "1"
fjall = "2"
tracing = "0.1"
tracing-subscriber = "0.3"

[dev-dependencies]
proptest = "1"
```

```bash
cargo build   # must succeed before you write a single line of logic
```

Skip Iceberg, Arrow, Parquet, object_store for now. Add them when you actually need cold storage. Same for `hnsw_rs`, `tantivy`, `ascent` — those are application concerns.

## Step 3: Prove the toolchain works on a real graph

Before writing anything novel, prove Leiden runs on Zachary's karate club. If you can't get *that* working you have no business writing the index.

```bash
mkdir -p data
curl -L -o data/karate.gml https://raw.githubusercontent.com/networkx/networkx/main/examples/graph/data/karate.gml
```

`src/lib.rs` — first thing, just load the graph and print node/edge counts:

```rust
use petgraph::graph::UnGraph;

pub fn load_karate(path: &str) -> UnGraph<u32, ()> {
    // simplest possible GML parse — pull edge pairs out
    let text = std::fs::read_to_string(path).unwrap();
    let mut g = UnGraph::<u32, ()>::new_undirected();
    let mut nodes = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("source ") {
            let src: u32 = rest.trim().parse().unwrap();
            let _ = *nodes.entry(src).or_insert_with(|| g.add_node(src));
        }
        // ... cheap; replace later with a real GML crate if needed
    }
    g
}

#[cfg(test)]
mod tests {
    #[test]
    fn karate_loads() {
        let g = super::load_karate("data/karate.gml");
        assert!(g.node_count() > 0);
    }
}
```

```bash
cargo test
```

This is the bar for "installed and working." Until this passes, nothing else matters.

## Step 4: Smallest working Leiden

Write the simplest correct Leiden in one file. Don't split into modules yet. Goal: produce *any* community partition with positive modularity on Zachary.

- Modularity of a partition: `Q = sum over communities of (e_ii - a_i^2)` where `e_ii` is fraction of edges inside community `i` and `a_i` is fraction of edges touching it.
- Local-moving phase: for each node, try moving it to each neighbor's community; keep the move that maximizes ΔQ.
- Iterate until no node moves.
- Skip the refinement + aggregation phases for the first cut — that's full Leiden. The first cut is just Louvain. You can upgrade to full Leiden after you see modularity numbers that look right.

Validate: Zachary partitions into 2–4 communities, modularity ≥ 0.38. If you see modularity ~0.4 you're correct enough to move on.

## Step 5: Smallest working hub detection

Degree-based only. Top-k% by degree gets flagged as hub. No betweenness centrality, no fancy graph-of-hubs construction yet.

```rust
pub fn detect_hubs(g: &UnGraph<u32, ()>, top_pct: f64) -> Vec<NodeIndex> {
    let mut by_deg: Vec<_> = g.node_indices()
        .map(|n| (n, g.edges(n).count()))
        .collect();
    by_deg.sort_by_key(|&(_, d)| std::cmp::Reverse(d));
    let cutoff = ((g.node_count() as f64) * top_pct).ceil() as usize;
    by_deg.into_iter().take(cutoff).map(|(n, _)| n).collect()
}
```

That's it. Argue about Brandes' algorithm later.

## Step 6: Persist to Fjall

Now you need the index on disk. This is where most of the production complexity lives but the API is simple:

```rust
use fjall::{Config, Keyspace, PartitionCreateOptions};

let keyspace = Config::new("data/swindex.fjall").open()?;
let uuid_to_cluster = keyspace.open_partition("uuid_to_cluster", PartitionCreateOptions::default())?;
uuid_to_cluster.insert(uuid.as_bytes(), cluster_id.to_le_bytes())?;
```

If `keyspace.open()` succeeds and you can read back what you wrote, storage is working. Worry about atomicity (write batches) only when you actually have concurrent writers.

## Step 7: Query — one end-to-end path

Pick the simplest possible query: "given a starting node, return all nodes in its cluster." That's it. No multi-hop, no pattern matching, no hub routing. Once that returns the right answer, build outward.

## Step 8: Verify the install end-to-end

A fresh user should be able to:

```bash
git clone <repo>
cd swindex
cargo test                           # green
cargo run --example karate           # prints community partition
```

If both work, the library is installed and functional. That is the bar for v0.0.1.

---

## What we are NOT doing yet

- No workspaces. Single crate until splitting earns its complexity.
- No Arrow/Parquet/Iceberg. Fjall only. Cold storage comes when hot storage hurts.
- No async/tokio. Leiden is CPU-bound; rayon is enough.
- No public API stabilization. The API will change. Don't ship 1.0.
- No CI yet. Get it working locally first.
- No license / crates.io publication. That's a release-day concern.
- No real estate, signed facts, MCP, REST. Those are applications, not the index.

## What "done" looks like for v0.0.1

1. `cargo test` is green.
2. Running Leiden on Zachary produces modularity ≥ 0.38.
3. Hub detection picks the top ~10% of nodes by degree.
4. Fjall round-trips `uuid → cluster_id`.
5. A demo prints "node X is in cluster Y, which contains N members."

After that, attack the next bottleneck — usually it's "the algorithm doesn't scale past 100K nodes" or "incremental updates corrupt cluster boundaries." Fix what's actually broken, not what the design doc predicts will be broken.

---

## If you're really stuck

Run this from scratch as a sanity check:

```bash
cd /tmp
cargo new --lib swindex-smoke
cd swindex-smoke
cargo add petgraph
cat > src/lib.rs <<'EOF'
use petgraph::graph::UnGraph;
#[test]
fn it_works() {
    let mut g = UnGraph::<i32, ()>::new_undirected();
    let a = g.add_node(1);
    let b = g.add_node(2);
    g.add_edge(a, b, ());
    assert_eq!(g.edge_count(), 1);
}
EOF
cargo test
```

If `cargo test` is green there, the toolchain is fine and everything from Step 3 onward is just typing. If it fails, fix Rust first — `rustup update`, check `$PATH`, restart the shell.
