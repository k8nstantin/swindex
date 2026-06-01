//! `cargo run --example schema_inspect -- <path-to-schema.sql>`
//!
//! Build a swindex from a `mysqldump --no-data` output, print the
//! detected clusters with their table names, and show a few example
//! `Similar` query results. The first real-world validation step
//! before designing Phase 2 rebalancing — does the algorithm produce
//! clusterings that make sense to a human looking at the schema?
//!
//! No assertion, no test framework — this prints human-readable output
//! you eyeball to judge whether the clustering is reasonable for *this*
//! schema. If the clusters look right, the static index works on real
//! data and Phase 2 is worth building. If they look wrong, we have a
//! foundation issue to fix first.
//!
//! # Why this is in `examples/` not `tests/`
//!
//! Schemas are private; `tests/` files end up in CI logs. `examples/`
//! is for ad-hoc runs against local fixtures. Output goes to stdout,
//! never to disk inside the repo.

use std::time::Instant;

use swindex::{QueryKind, SliceSource, SqlDumpSource, SwIndex};
use tempfile::TempDir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Optional: set RUST_LOG=swindex=info for tracing output.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let path = std::env::args().nth(1).ok_or(
        "usage: cargo run --example schema_inspect -- <path-to-schema.sql>\n\
         (the schema file must live outside the repo or under data/ — both are gitignored)",
    )?;

    println!("=== swindex schema inspect ===");
    println!("Reading: {path}");

    // Step 1: parse the dump.
    let t0 = Instant::now();
    let source = SqlDumpSource::from_path(&path)?;
    let parse_ms = t0.elapsed().as_millis();
    println!(
        "Parsed: {} tables, {} foreign keys ({} ms)",
        source.table_count(),
        source.fk_count(),
        parse_ms
    );

    if source.table_count() == 0 {
        eprintln!("No tables found. Is this a `mysqldump --no-data` output?");
        return Ok(());
    }

    // Step 2: convert the source into vec'd nodes/edges so we can
    // both build the index AND keep the name->uuid map for output.
    let nodes: Vec<_> = swindex::GraphSource::nodes(&source).collect();
    let edges: Vec<_> = swindex::GraphSource::edges(&source).collect();
    let slice = SliceSource::new(&nodes, &edges);

    // Step 3: build the index in a tempdir (cleaned up on exit).
    let dir = TempDir::new()?;
    let mut idx = SwIndex::open(dir.path())?;
    let t1 = Instant::now();
    let stats = idx.build_from_source(&slice)?;
    let build_ms = t1.elapsed().as_millis();
    println!(
        "Built: {} clusters, {} regions, {} hubs ({} ms)",
        stats.clusters, stats.regions, stats.hubs, build_ms
    );

    // Step 4: invert the uuid->name map so we can label each cluster
    // member back to its human-readable table name.
    let mut uuid_to_name: std::collections::BTreeMap<_, &str> = std::collections::BTreeMap::new();
    for (name, uuid) in source.tables_named() {
        uuid_to_name.insert(uuid, name);
    }

    // Step 5: dump every cluster's members, sorted by cluster id.
    println!("\n=== Clusters ===");
    #[allow(clippy::cast_possible_truncation)]
    let clusters = stats.clusters as u32;
    for cid in 0..clusters {
        let Some(members) = idx.cluster_members(cid)? else {
            continue;
        };
        let (size, hub_count) = idx.cluster_meta(cid)?.unwrap_or((0, 0));
        let mut names: Vec<&str> = members
            .iter()
            .filter_map(|u| uuid_to_name.get(u).copied())
            .collect();
        names.sort_unstable();
        println!(
            "Cluster {cid:>3}  ({size:>3} tables, {hub_count} hubs):  {}",
            names.join(", ")
        );
    }

    // Step 6: optional — sample a few seeds and show Similar() results.
    // Take a deterministic sample (the first 5 tables by name) so the
    // output is reproducible across runs.
    println!("\n=== Similar(seed, limit=10) — first 5 tables by name ===");
    let mut sample_names: Vec<(&str, swindex::Uuid7)> = source.tables_named().collect();
    sample_names.sort_by(|a, b| a.0.cmp(b.0));
    for (name, uuid) in sample_names.iter().take(5) {
        let result = idx.query(QueryKind::Similar {
            start: *uuid,
            limit: 10,
        })?;
        let mut peer_names: Vec<&str> = result
            .uuids
            .iter()
            .filter_map(|u| uuid_to_name.get(u).copied())
            .collect();
        peer_names.sort_unstable();
        println!(
            "  {name:<30}  -> {} hits over {} clusters, {} hubs visited",
            peer_names.len(),
            result.stats.clusters_visited,
            result.stats.hubs_visited
        );
        if !peer_names.is_empty() {
            println!("      similar to: {}", peer_names.join(", "));
        }
    }

    println!("\nDone.");
    Ok(())
}
