//! `swindex` — shell-usable CLI for the swindex property-graph index
//! (issue #58).
//!
//! The user-facing face of the library. Wraps `SqlDumpSource`,
//! `SwIndex::build_from_source`, and `SwIndex::query_by_label` into
//! a familiar `<verb> <object>` command interface so you can build
//! and query an index from a shell without writing Rust.
//!
//! # Usage examples
//!
//! ```bash
//! # Build from a mysqldump output.
//! swindex build --from-sql data/schema.sql --to data/idx.fjall
//!
//! # Query.
//! swindex similar core_manager.tbl_CallDetails --index data/idx.fjall --limit 10
//! swindex same-cluster core_manager.tbl_Customer --index data/idx.fjall
//! swindex info --index data/idx.fjall
//! swindex clusters --index data/idx.fjall --min-size 5
//! swindex drift --index data/idx.fjall
//! swindex tables --index data/idx.fjall --grep tbl_Call
//! ```
//!
//! # Logging
//!
//! Set `RUST_LOG=swindex=info` (or `=debug`, `=trace`) to surface the
//! tracing spans the library emits. Default is `off` — the CLI's own
//! output is plain stdout and shouldn't be polluted by spans.

#![allow(clippy::print_stdout, clippy::print_stderr)] // explicit CLI output

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use swindex::{QueryKindByLabel, SliceSource, SqlDumpSource, SwIndex};

/// Default index location when `--index` isn't passed. Sits next to
/// the invocation cwd — fine for ad-hoc use; pass `--index` for
/// shared/production locations.
const DEFAULT_INDEX_PATH: &str = "./swindex.fjall";

#[derive(Parser, Debug)]
#[command(
    name = "swindex",
    version,
    about = "swindex — hierarchical property-graph index CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build an index from a `mysqldump --no-data` SQL file.
    Build {
        /// Path to the SQL dump.
        #[arg(long, value_name = "PATH")]
        from_sql: PathBuf,
        /// Where to write the index (a directory). Created if missing.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        to: PathBuf,
    },
    /// Return tables structurally similar to a seed table.
    Similar {
        /// The seed table's label (e.g. `db.tbl_customer`).
        name: String,
        /// Path to the index directory.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        index: PathBuf,
        /// Maximum number of results.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Return every table in the same cluster as a seed.
    SameCluster {
        /// The seed table's label.
        name: String,
        /// Path to the index directory.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        index: PathBuf,
    },
    /// Print index-wide stats (counts of nodes, clusters, hubs).
    Info {
        /// Path to the index directory.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        index: PathBuf,
    },
    /// List all clusters with their member tables.
    Clusters {
        /// Path to the index directory.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        index: PathBuf,
        /// Only show clusters with at least this many members.
        #[arg(long, default_value_t = 1)]
        min_size: u32,
        /// Maximum number of clusters to print.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Show Phase 1 incremental-maintenance drift report.
    Drift {
        /// Path to the index directory.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        index: PathBuf,
    },
    /// List all known table labels, optionally filtered by substring.
    Tables {
        /// Path to the index directory.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_INDEX_PATH)]
        index: PathBuf,
        /// Only show labels containing this substring.
        #[arg(long, value_name = "PATTERN")]
        grep: Option<String>,
        /// Maximum number of labels to print.
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
}

fn main() -> ExitCode {
    // tracing spans the library emits are dropped silently here
    // (no subscriber). Users who want them can wrap with their own
    // subscriber init — we don't pull `tracing-subscriber` as a
    // production dep just for the CLI.
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Build { from_sql, to } => cmd_build(&from_sql, &to),
        Command::Similar { name, index, limit } => cmd_similar(&name, &index, limit),
        Command::SameCluster { name, index } => cmd_same_cluster(&name, &index),
        Command::Info { index } => cmd_info(&index),
        Command::Clusters {
            index,
            min_size,
            limit,
        } => cmd_clusters(&index, min_size, limit),
        Command::Drift { index } => cmd_drift(&index),
        Command::Tables { index, grep, limit } => cmd_tables(&index, grep.as_deref(), limit),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("swindex: {e}");
            ExitCode::FAILURE
        }
    }
}

// ===========================================================================
// Subcommand implementations.
// ===========================================================================

fn cmd_build(
    from_sql: &std::path::Path,
    to: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Reading SQL dump: {}", from_sql.display());
    let source = SqlDumpSource::from_path(from_sql)?;
    println!(
        "Parsed: {} tables, {} FKs, {} procedure-co-occurrence pairs ({} total edges)",
        source.table_count(),
        source.fk_count(),
        source.proc_pair_count(),
        source.edge_count(),
    );

    // Vec-ify so we can hand the same data to a SliceSource (the
    // current `build_from_source` requires `&G: GraphSource`; the
    // raw `SqlDumpSource` reference would also work, but using
    // `SliceSource` keeps the example consistent with the
    // schema_inspect path).
    let nodes: Vec<_> = swindex::GraphSource::nodes(&source).collect();
    let edges: Vec<_> = swindex::GraphSource::edges(&source).collect();

    // Wrap so we can re-emit labels through the LabeledSlice.
    let slice = LabeledSlice {
        nodes,
        edges,
        labels: collect_labels(&source),
    };

    println!("Building index at: {}", to.display());
    let mut idx = SwIndex::open(to)?;
    let stats = idx.build_from_source(&slice)?;
    println!(
        "Built: {} clusters, {} regions, {} hubs ({} nodes)",
        stats.clusters, stats.regions, stats.hubs, stats.nodes
    );
    Ok(())
}

fn cmd_similar(
    name: &str,
    index: &std::path::Path,
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let idx = SwIndex::open(index)?;
    let result = idx.query_by_label(QueryKindByLabel::Similar {
        start: name.to_string(),
        limit,
    })?;

    if result.labels.is_empty() {
        // Disambiguate "no label resolved" from "table found but no neighbors".
        if idx.uuid_of_label(name)?.is_none() {
            eprintln!("swindex: no table named {name:?} in the index");
            return Ok(());
        }
        println!("No similar tables found.");
        return Ok(());
    }

    println!("Similar to {name}:");
    for label in &result.labels {
        println!("  {label}");
    }
    println!(
        "\n{} results · {} clusters visited · {} hubs visited",
        result.labels.len(),
        result.stats.clusters_visited,
        result.stats.hubs_visited
    );
    Ok(())
}

fn cmd_same_cluster(name: &str, index: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let idx = SwIndex::open(index)?;
    let result = idx.query_by_label(QueryKindByLabel::SameCluster {
        start: name.to_string(),
    })?;

    if result.labels.is_empty() {
        if idx.uuid_of_label(name)?.is_none() {
            eprintln!("swindex: no table named {name:?} in the index");
            return Ok(());
        }
        println!("Cluster is empty (singleton).");
        return Ok(());
    }

    println!("Same cluster as {name}:");
    for label in &result.labels {
        println!("  {label}");
    }
    println!("\n{} tables in this cluster", result.labels.len());
    Ok(())
}

fn cmd_info(index: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let idx = SwIndex::open(index)?;
    let stats = idx.stats();
    println!("Index:    {}", index.display());
    println!("Nodes:    {}", stats.nodes);
    println!("Clusters: {}", stats.clusters);
    println!("Hubs:     {}", stats.hubs);
    Ok(())
}

fn cmd_clusters(
    index: &std::path::Path,
    min_size: u32,
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let idx = SwIndex::open(index)?;
    let stats = idx.stats();

    let mut printed = 0_usize;
    let mut skipped_small = 0_usize;
    #[allow(clippy::cast_possible_truncation)]
    let cluster_count = stats.clusters as u32;
    for cid in 0..cluster_count {
        let Some((size, hub_count)) = idx.cluster_meta(cid)? else {
            continue;
        };
        if size < min_size {
            skipped_small += 1;
            continue;
        }
        if printed >= limit {
            break;
        }
        printed += 1;

        let members = idx.cluster_members(cid)?.unwrap_or_default();
        let mut labels = Vec::with_capacity(members.len());
        for u in &members {
            let label = idx.label_of(*u)?.unwrap_or_else(|| u.as_uuid().to_string());
            labels.push(label);
        }
        labels.sort();

        println!(
            "Cluster {cid:>4}  ({size:>4} tables, {hub_count} hubs):  {}",
            labels.join(", ")
        );
    }

    let total_below_threshold = if min_size > 1 { skipped_small } else { 0 };
    if total_below_threshold > 0 {
        println!(
            "\n{printed} clusters shown · {total_below_threshold} below --min-size={min_size}"
        );
    } else {
        println!("\n{printed} clusters shown");
    }
    Ok(())
}

fn cmd_drift(index: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let idx = SwIndex::open(index)?;
    let report = idx.drift_report()?;
    println!(
        "Drift report ({} clusters tracked):",
        report.cluster_count()
    );
    let mut entries: Vec<_> = report.per_cluster.iter().collect();
    entries.sort_by_key(|(_, d)| std::cmp::Reverse(d.delta_inserts));
    for (cid, drift) in entries.iter().take(50) {
        if drift.delta_inserts > 0 {
            println!(
                "  cluster {cid:>4}: generation={}  delta_inserts={}",
                drift.generation, drift.delta_inserts
            );
        }
    }
    println!(
        "\nTotal inserts since last rebalance: {}",
        report.total_inserts()
    );
    Ok(())
}

fn cmd_tables(
    index: &std::path::Path,
    grep: Option<&str>,
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let idx = SwIndex::open(index)?;
    let stats = idx.stats();

    // Iterate cluster_members to discover every uuid, then map to
    // its label. Yes, this walks the index — but for human-scale
    // schemas (hundreds to low tens of thousands of tables) it's
    // fast enough; we can add a dedicated label scan path later if
    // we end up calling this on million-table indexes.
    let mut printed = 0_usize;
    let mut filtered_out = 0_usize;
    #[allow(clippy::cast_possible_truncation)]
    let cluster_count = stats.clusters as u32;
    let mut labels: Vec<String> = Vec::new();
    for cid in 0..cluster_count {
        let Some(members) = idx.cluster_members(cid)? else {
            continue;
        };
        for u in members {
            if let Some(label) = idx.label_of(u)? {
                labels.push(label);
            }
        }
    }
    labels.sort();
    labels.dedup();

    for label in labels {
        if let Some(pat) = grep {
            if !label.contains(pat) {
                filtered_out += 1;
                continue;
            }
        }
        if printed >= limit {
            break;
        }
        println!("  {label}");
        printed += 1;
    }
    if let Some(pat) = grep {
        println!("\n{printed} tables matching {pat:?} ({filtered_out} filtered out)");
    } else {
        println!("\n{printed} tables");
    }
    Ok(())
}

// ===========================================================================
// Helpers.
// ===========================================================================

/// Collect every `(Uuid7, label)` pair from a `SqlDumpSource` so we
/// can hand it to a `LabeledSlice` for the build step. Used by
/// `cmd_build`.
fn collect_labels(source: &SqlDumpSource) -> std::collections::BTreeMap<swindex::Uuid7, String> {
    let mut out = std::collections::BTreeMap::new();
    for (name, uuid) in source.tables_named() {
        out.insert(uuid, name.to_string());
    }
    out
}

/// A thin `GraphSource` wrapper that holds pre-collected nodes/edges
/// plus a uuid → label map. Lets `cmd_build` use a borrowed slice
/// while still emitting labels via the trait's `label_of` method.
struct LabeledSlice {
    nodes: Vec<swindex::Node>,
    edges: Vec<swindex::Edge>,
    labels: std::collections::BTreeMap<swindex::Uuid7, String>,
}

impl swindex::GraphSource for LabeledSlice {
    fn nodes(&self) -> impl Iterator<Item = swindex::Node> + '_ {
        self.nodes.iter().cloned()
    }
    fn edges(&self) -> impl Iterator<Item = swindex::Edge> + '_ {
        self.edges.iter().cloned()
    }
    fn node_count_hint(&self) -> Option<usize> {
        Some(self.nodes.len())
    }
    fn edge_count_hint(&self) -> Option<usize> {
        Some(self.edges.len())
    }
    fn label_of(&self, node_id: swindex::Uuid7) -> Option<String> {
        self.labels.get(&node_id).cloned()
    }
}

/// `LabeledSlice` borrows nothing — the unused-import suppression
/// for `SliceSource` is intentional, kept for symmetry with future
/// alternative paths.
#[allow(dead_code)]
fn _silence_unused(_: SliceSource<'_>) {}
