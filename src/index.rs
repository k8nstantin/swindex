//! The persisted public index — `SwIndex`.
//!
//! # What this module ships
//!
//! [`SwIndex`] — the on-disk, query-time face of the four-layer
//! architecture. It wraps a Fjall keyspace with seven partitions
//! ("keyspaces" in the design doc's vocabulary) that hold the
//! structural metadata produced by Layers 0–3 plus the incremental-
//! maintenance bookkeeping added in Phase 1 (issue #52):
//!
//! | Partition | Key | Value | Purpose |
//! |-----------|-----|-------|---------|
//! | `uuid_to_cluster` | `Uuid7` (16 B) | `ClusterId` (u32 LE, 4 B) | "Which cluster is this node in?" |
//! | `uuid_to_region`  | `Uuid7` (16 B) | `RegionId` (u32 LE, 4 B)  | "Which region is this node in?" |
//! | `uuid_is_hub`     | `Uuid7` (16 B) | `bool` (1 B)              | Hub flag, answered before expensive walks |
//! | `hub_neighbors`   | hub `Uuid7`    | length-prefixed `Vec<(Uuid7, f32)>` | The hub-graph adjacency |
//! | `cluster_members` | `ClusterId` (u32 LE) | length-prefixed `Vec<Uuid7>` | "Who's in this cluster?" |
//! | `cluster_meta`    | `ClusterId` (u32 LE) | `{size: u32, hub_count: u32}` (8 B) | Size + hub count per cluster |
//! | `cluster_drift`   | `ClusterId` (u32 LE) | `{generation: u64, delta_inserts: u32}` (12 B) | Per-cluster insert pressure since last rebuild (Phase 1) |
//!
//! # On-disk footprint
//!
//! Per-node values total **40 + node_bytes_in_clusters + hub_bytes** bytes:
//! `uuid → cluster` (4 B) + `uuid → region` (4 B) + `uuid → is_hub` (1 B)
//! plus the node's appearance in `cluster_members` (16 B) — call it
//! **~25 B per node** for the structural metadata. On a 250K-node graph
//! that's ~6 MB before LSM overhead — well under 2-5% of any realistic
//! underlying-data footprint.
//!
//! # Atomicity
//!
//! [`SwIndex::build_from_source`] writes every partition in a single
//! Fjall [`fjall::Batch`] and commits it atomically. Either the whole
//! build is visible or none of it is — there's no "half-built" state a
//! query could observe.
//!
//! # What this module deliberately doesn't do (yet)
//!
//! * **Query planner.** [`SwIndex::cluster_of`] / [`region_of`] / etc.
//!   are simple point lookups today. The four-layer routing (region →
//!   hub-graph → cluster → within-cluster) lives in issue #26 and a
//!   future PR.
//! * **Incremental maintenance.** Phase 1 (issue #52) added the
//!   *interface*: [`SwIndex::insert_node`], [`SwIndex::drift_report`],
//!   and [`SwIndex::maintain`] with [`crate::maintenance::NeverRebalance`]
//!   as the stub policy that does nothing. **Real rebalancing
//!   (threshold-driven Phase 2, full Ada-IVF Phase 3+) is still issue
//!   #27.** Drift accumulates indefinitely under `NeverRebalance`; for
//!   quality preservation today, periodically `build_from_source` from
//!   scratch.
//! * **Time-travel** (`query_as_of`). Bitemporal history tables in
//!   Parquet are issue #29, also post-v0.1.
//!
//! # Observability
//!
//! All public methods emit [`tracing`](https://docs.rs/tracing) spans
//! at `info` level (`open`, `build_from_source`, `query`) with
//! debug-level sub-spans per build pipeline phase
//! (`graph`, `leiden`, `regions`, `hubs`, `hub_graph`, `persist`).
//! Spans carry useful fields — graph size, cluster count, query stats —
//! so a `tracing-subscriber` consumer can correlate the planner's
//! routing decisions with wall-clock cost.
//!
//! No subscriber is wired up by default; the caller picks one. To see
//! the spans during development:
//!
//! ```no_run
//! use tracing_subscriber::EnvFilter;
//! tracing_subscriber::fmt()
//!     .with_env_filter(EnvFilter::from_default_env())
//!     .init();
//! // RUST_LOG=swindex=debug cargo run …
//! ```

use crate::community::leiden;
use crate::graph::{Graph, GraphError};
use crate::hub::HubSet;
use crate::hub_graph::HubGraph;
use crate::id::Uuid7;
use crate::maintenance::{ClusterDrift, DriftReport, MaintenancePolicy, MaintenanceReport};
use crate::node::Node;
use crate::region::RegionGraph;
use crate::source::GraphSource;

use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use tracing::{debug, debug_span, info, info_span};
use uuid::Uuid;

// Default fraction of nodes flagged as hubs by `build_from_source`.
// 10% is a reasonable starting point per the design doc's 0.1–5% range
// for graphs at academic-fixture scale; production tuning lives at the
// caller via a future `SwConfig` parameter.
const DEFAULT_HUB_FRACTION: f64 = 0.10;
// Hub-graph k_hop default. Per `DESIGN.md` line 102.
const DEFAULT_HUB_GRAPH_K_HOP: usize = 3;

/// Version byte prepended to every variable-length value written by
/// this build. Encoders write it; decoders refuse anything else. Bump
/// when the binary layout of `cluster_members`, `hub_neighbors`, or
/// `cluster_meta` changes — and remember to keep `read_format_byte`
/// in sync (or fan it out into version-specific decoders).
///
/// Fixed-width values (`uuid_to_cluster`, `uuid_to_region`,
/// `uuid_is_hub`) deliberately have no version byte: they're trivially
/// evolvable by widening the value type and re-detecting at open time.
const FORMAT_V1: u8 = 0x01;

/// The persisted small-world property-graph index.
///
/// `SwIndex` wraps a Fjall keyspace plus seven partitions. Open one at
/// a directory path with [`SwIndex::open`]; populate it with
/// [`SwIndex::build_from_source`]; query it with the various read
/// accessors. The same path reopened later yields the same data
/// (durability invariant tested in `round_trip_via_close_and_reopen`).
pub struct SwIndex {
    /// The Fjall keyspace owning all partitions. Held as the last
    /// field so its `Drop` runs after all `PartitionHandle` references.
    keyspace: Keyspace,
    uuid_to_cluster: PartitionHandle,
    uuid_to_region: PartitionHandle,
    uuid_is_hub: PartitionHandle,
    hub_neighbors: PartitionHandle,
    cluster_members: PartitionHandle,
    cluster_meta: PartitionHandle,
    /// Per-cluster drift state — `{generation, delta_inserts}` keyed
    /// by `ClusterId` (u32 LE). Written by `build_from_source`
    /// (initial generation 0, delta_inserts 0 for every cluster) and
    /// updated by `insert_node`. Read by `drift_report` and
    /// `maintain`. New in Phase 1 (issue #52).
    cluster_drift: PartitionHandle,
}

/// Errors from any [`SwIndex`] operation.
#[derive(Debug)]
pub enum SwIndexError {
    /// IO error reading or writing the underlying filesystem.
    Io(std::io::Error),
    /// Error from the Fjall LSM engine — partition not openable, write
    /// failure, recovery failure, etc.
    Fjall(fjall::Error),
    /// Error building the in-memory graph from the source (dangling
    /// edge, malformed input).
    Graph(GraphError),
    /// On-disk data is shaped differently than expected — a corrupted
    /// keyspace, truncated value, or otherwise malformed payload. The
    /// string carries detail for the operator. For version-mismatch
    /// specifically see [`SwIndexError::UnsupportedFormat`].
    Corruption(String),
    /// A persisted value carries a format-version byte this build does
    /// not know how to decode. Written by a newer swindex release into
    /// a partition this older build is now trying to read. Distinct
    /// from [`SwIndexError::Corruption`] so callers can route
    /// upgrade-prompts separately from data-integrity alerts.
    UnsupportedFormat {
        /// The version byte we found at the head of the payload.
        found: u8,
        /// The partition / encoding we were trying to decode, for
        /// operator-facing detail.
        context: &'static str,
    },
}

impl fmt::Display for SwIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwIndexError::Io(e) => write!(f, "swindex io error: {e}"),
            SwIndexError::Fjall(e) => write!(f, "swindex fjall error: {e}"),
            SwIndexError::Graph(e) => write!(f, "swindex graph error: {e}"),
            SwIndexError::Corruption(s) => write!(f, "swindex on-disk corruption: {s}"),
            SwIndexError::UnsupportedFormat { found, context } => {
                let known = FORMAT_V1;
                write!(
                    f,
                    "swindex unsupported format: {context} carries version byte 0x{found:02x}, \
                     this build only knows 0x{known:02x}. Upgrade swindex or rebuild the index."
                )
            }
        }
    }
}

impl std::error::Error for SwIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwIndexError::Io(e) => Some(e),
            SwIndexError::Fjall(e) => Some(e),
            SwIndexError::Graph(e) => Some(e),
            SwIndexError::Corruption(_) | SwIndexError::UnsupportedFormat { .. } => None,
        }
    }
}

impl From<std::io::Error> for SwIndexError {
    fn from(e: std::io::Error) -> Self {
        SwIndexError::Io(e)
    }
}

impl From<fjall::Error> for SwIndexError {
    fn from(e: fjall::Error) -> Self {
        SwIndexError::Fjall(e)
    }
}

impl From<GraphError> for SwIndexError {
    fn from(e: GraphError) -> Self {
        SwIndexError::Graph(e)
    }
}

/// Summary statistics returned by [`SwIndex::build_from_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildStats {
    /// Total nodes ingested.
    pub nodes: usize,
    /// Number of clusters detected by Leiden.
    pub clusters: usize,
    /// Number of regions detected by the recursive Leiden pass.
    pub regions: usize,
    /// Number of nodes flagged as hubs.
    pub hubs: usize,
}

/// What kind of query to run. Each variant maps to a specific routing
/// path through the four-layer index.
///
/// More query kinds will be added as use cases land. For v0.1 the two
/// here exercise the load-bearing routing primitives:
///
/// * [`QueryKind::SameCluster`] — a single-layer lookup. Resolves the
///   start node's cluster (Layer-1 lookup via Fjall) and returns its
///   members (Layer-1 fan-out). Demonstrates the cheapest possible
///   query path; latency is dominated by the cluster-members fetch.
/// * [`QueryKind::Similar`] — a two-layer query. Resolves the start's
///   cluster, returns its members first (priority 0), then expands to
///   neighboring clusters by walking the hub graph (Layer 2), then
///   collecting those clusters' members (priority 1). Truncated at
///   `limit`. Demonstrates Layer-0 → Layer-1 → Layer-2 → Layer-1
///   routing — the typical "find me things related to X" query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// Return every uuid in the same cluster as `start`. The start
    /// uuid itself is included.
    SameCluster {
        /// The seed node whose cluster will be returned.
        start: Uuid7,
    },
    /// Return up to `limit` uuids structurally similar to `start`,
    /// in priority order: same cluster first, then neighboring
    /// clusters reached via the hub graph.
    Similar {
        /// The seed node.
        start: Uuid7,
        /// Maximum number of results to return.
        limit: usize,
    },
}

/// Per-query observability — useful for measuring whether the routing
/// took the expected path and for benchmarking. Not stable API yet;
/// fields may be added or renamed in future versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryStats {
    /// Number of distinct clusters whose members were inspected.
    pub clusters_visited: usize,
    /// Number of hub-graph edges followed during routing.
    pub hubs_visited: usize,
}

/// Result of a query against [`SwIndex`]. Contains the matched uuids
/// (in priority order — same cluster first, then expanded clusters)
/// and routing telemetry.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Matched uuids in priority order (same cluster first when the
    /// query is `Similar`; for `SameCluster` the order is sorted by
    /// `Uuid7` since `cluster_members` returns them that way).
    pub uuids: Vec<Uuid7>,
    /// Routing telemetry.
    pub stats: QueryStats,
}

/// Counts of what's currently stored in the index — for introspection
/// and benchmarking. Cheap to compute (uses Fjall's
/// `approximate_len`); not authoritative under concurrent writes but
/// fine for stats panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwStats {
    /// Approximate number of nodes (entries in `uuid_to_cluster`).
    pub nodes: usize,
    /// Approximate number of clusters (entries in `cluster_members`).
    pub clusters: usize,
    /// Approximate number of hubs (entries in `hub_neighbors`).
    pub hubs: usize,
}

impl SwIndex {
    /// Open (or create) a `SwIndex` at the given filesystem path.
    ///
    /// The path is a directory; Fjall manages its internal layout
    /// inside it. If the directory doesn't exist Fjall creates it; if
    /// it exists and contains a prior `SwIndex`, the partitions are
    /// recovered.
    ///
    /// # Errors
    ///
    /// * [`SwIndexError::Fjall`] if Fjall can't open the keyspace or
    ///   recover the partitions (filesystem permission, corruption).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SwIndexError> {
        let path_ref = path.as_ref();
        let _span = info_span!("swindex.open", path = ?path_ref).entered();
        let keyspace = Config::new(path_ref).open()?;
        let opts = PartitionCreateOptions::default();
        // Every partition is opened by name; if it doesn't exist yet
        // Fjall creates it transparently. Calling `open` twice on the
        // same path returns equivalent handles.
        let uuid_to_cluster = keyspace.open_partition("uuid_to_cluster", opts.clone())?;
        let uuid_to_region = keyspace.open_partition("uuid_to_region", opts.clone())?;
        let uuid_is_hub = keyspace.open_partition("uuid_is_hub", opts.clone())?;
        let hub_neighbors = keyspace.open_partition("hub_neighbors", opts.clone())?;
        let cluster_members = keyspace.open_partition("cluster_members", opts.clone())?;
        let cluster_meta = keyspace.open_partition("cluster_meta", opts.clone())?;
        // cluster_drift is created lazily — indexes built before Phase 1
        // simply have an empty partition until the next build_from_source
        // populates it. Decoders treat "no entry" as `{generation: 0,
        // delta_inserts: 0}` so reads stay valid in the meantime.
        let cluster_drift = keyspace.open_partition("cluster_drift", opts)?;
        debug!("keyspace + 7 partitions opened");
        Ok(Self {
            keyspace,
            uuid_to_cluster,
            uuid_to_region,
            uuid_is_hub,
            hub_neighbors,
            cluster_members,
            cluster_meta,
            cluster_drift,
        })
    }

    /// Build the index from a [`GraphSource`]. Runs the full Layer-0..3
    /// pipeline (Graph → Leiden → Hubs → HubGraph → RegionGraph) and
    /// commits every resulting structure to disk in a single atomic
    /// Fjall batch.
    ///
    /// **This rebuilds from scratch.** Calling `build_from_source` on a
    /// keyspace that already contains data overwrites the existing
    /// entries for every UUID present in the new source; UUIDs from
    /// prior builds that aren't in the new source remain (they aren't
    /// explicitly removed). For a clean rebuild, open a fresh path.
    ///
    /// # Errors
    ///
    /// * [`SwIndexError::Graph`] — the source produced a dangling edge.
    /// * [`SwIndexError::Fjall`] — Fjall write failure.
    /// * [`SwIndexError::Corruption`] — a cluster or region id exceeded
    ///   the u32 storage limit (would only happen with > 4 B
    ///   communities; not realistically reachable).
    // Long but linear: 5 pipeline phases inline so the tracing spans
    // line up cleanly with the code structure. Splitting into helper
    // methods would obscure that mapping.
    #[allow(clippy::too_many_lines)]
    pub fn build_from_source<G: GraphSource>(
        &mut self,
        source: &G,
    ) -> Result<BuildStats, SwIndexError> {
        let _span = info_span!("swindex.build").entered();

        // Step 1: in-memory Graph (Layer 0).
        let graph = {
            let _phase = debug_span!("swindex.build.graph").entered();
            Graph::from_source(source)?
        };
        debug!(nodes = graph.node_count(), "layer 0 — graph built");

        // Step 2: Leiden clusters (Layer 1).
        let clusters = {
            let _phase = debug_span!("swindex.build.leiden").entered();
            leiden(&graph)
        };
        debug!(
            communities = clusters.community_count(),
            "layer 1 — leiden done"
        );

        // Step 3: regions (Layer 3 — recursive Leiden over clusters).
        let regions = {
            let _phase = debug_span!("swindex.build.regions").entered();
            RegionGraph::build(&graph, &clusters)
        };
        debug!(regions = regions.region_count(), "layer 3 — regions done");

        // Step 4: hub set (Layer 2a) + hub graph (Layer 2b).
        let hubs = {
            let _phase = debug_span!("swindex.build.hubs").entered();
            HubSet::from_top_fraction(&graph, DEFAULT_HUB_FRACTION)
        };
        debug!(hubs = hubs.len(), "layer 2a — hub set");

        let hub_graph = {
            let _phase = debug_span!("swindex.build.hub_graph").entered();
            HubGraph::build(&graph, &hubs, DEFAULT_HUB_GRAPH_K_HOP)
        };
        debug!(edges = hub_graph.edge_count(), "layer 2b — hub graph");

        // Step 5: persist everything via one atomic Fjall batch.
        let _persist = debug_span!("swindex.build.persist").entered();
        let mut batch = self.keyspace.batch();

        for u in 0..graph.node_count() {
            let uuid = graph.node_id(u);
            let cluster_id_usize = clusters.community_of(u);
            let cluster_id = u32::try_from(cluster_id_usize)
                .map_err(|_| SwIndexError::Corruption("cluster id exceeds u32 range".into()))?;
            let region_id_usize = regions.region_of_cluster(cluster_id_usize).unwrap_or(0);
            let region_id = u32::try_from(region_id_usize)
                .map_err(|_| SwIndexError::Corruption("region id exceeds u32 range".into()))?;

            // Key = Uuid7 raw bytes (16 B); value = LE-encoded u32.
            batch.insert(
                &self.uuid_to_cluster,
                uuid.as_bytes().as_slice(),
                cluster_id.to_le_bytes().as_slice(),
            );
            batch.insert(
                &self.uuid_to_region,
                uuid.as_bytes().as_slice(),
                region_id.to_le_bytes().as_slice(),
            );
            batch.insert(
                &self.uuid_is_hub,
                uuid.as_bytes().as_slice(),
                [u8::from(hubs.contains(u))].as_slice(),
            );
        }

        // Hub neighbors: serialize each hub's adjacency list.
        for hub_idx in hubs.iter() {
            let hub_uuid = graph.node_id(hub_idx);
            let neighbors: Vec<(Uuid7, f32)> = hub_graph
                .neighbors(hub_idx)
                .map(|(n_idx, w)| {
                    // f64 -> f32 narrowing is intentional. Hub-graph
                    // weights are inverse-hop-distance and easily fit
                    // in f32; saving 4 bytes per entry adds up at scale.
                    #[allow(clippy::cast_possible_truncation)]
                    let w32 = w as f32;
                    (graph.node_id(n_idx), w32)
                })
                .collect();
            let buf = encode_hub_neighbors(&neighbors)?;
            batch.insert(&self.hub_neighbors, hub_uuid.as_bytes().as_slice(), buf);
        }

        // Cluster members + cluster meta.
        for (cluster_id_usize, members) in clusters.buckets().iter().enumerate() {
            let cluster_id = u32::try_from(cluster_id_usize)
                .map_err(|_| SwIndexError::Corruption("cluster id exceeds u32 range".into()))?;
            let member_uuids: Vec<Uuid7> = members.iter().map(|&u| graph.node_id(u)).collect();
            let members_buf = encode_uuid_vec(&member_uuids)?;
            batch.insert(
                &self.cluster_members,
                cluster_id.to_le_bytes().as_slice(),
                members_buf,
            );

            let hub_count_usize = members.iter().filter(|&&u| hubs.contains(u)).count();
            let size_u32 = u32::try_from(members.len())
                .map_err(|_| SwIndexError::Corruption("cluster size exceeds u32".into()))?;
            let hub_count_u32 = u32::try_from(hub_count_usize)
                .map_err(|_| SwIndexError::Corruption("hub count exceeds u32".into()))?;
            batch.insert(
                &self.cluster_meta,
                cluster_id.to_le_bytes().as_slice(),
                encode_cluster_meta(size_u32, hub_count_u32),
            );
            // Phase 1: initialize cluster drift to {generation: 0,
            // delta_inserts: 0} for every cluster created by this
            // build. Subsequent `insert_node` calls increment
            // delta_inserts; future `maintain` calls bump generation
            // when a cluster is rebalanced.
            batch.insert(
                &self.cluster_drift,
                cluster_id.to_le_bytes().as_slice(),
                encode_cluster_drift(0, 0),
            );
        }

        // Atomic commit + fsync. After this returns, a fresh open() of
        // the same path will see exactly this build.
        batch.commit()?;
        self.keyspace.persist(PersistMode::SyncAll)?;
        debug!("layer all — committed + fsynced");

        let stats = BuildStats {
            nodes: graph.node_count(),
            clusters: clusters.community_count(),
            regions: regions.region_count(),
            hubs: hubs.len(),
        };
        info!(
            nodes = stats.nodes,
            clusters = stats.clusters,
            regions = stats.regions,
            hubs = stats.hubs,
            "build complete"
        );
        Ok(stats)
    }

    // ---- Read accessors ----

    /// The cluster id of a node, or `None` if the UUID is unknown.
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] for read failures;
    /// [`SwIndexError::Corruption`] if the stored value isn't exactly
    /// 4 bytes (indicates a format-version mismatch).
    pub fn cluster_of(&self, uuid: Uuid7) -> Result<Option<u32>, SwIndexError> {
        let raw = self.uuid_to_cluster.get(uuid.as_bytes().as_slice())?;
        raw.map(|b| decode_u32(&b, "cluster_of")).transpose()
    }

    /// The region id of a node, or `None` if the UUID is unknown.
    ///
    /// # Errors
    ///
    /// Same as [`Self::cluster_of`].
    pub fn region_of(&self, uuid: Uuid7) -> Result<Option<u32>, SwIndexError> {
        let raw = self.uuid_to_region.get(uuid.as_bytes().as_slice())?;
        raw.map(|b| decode_u32(&b, "region_of")).transpose()
    }

    /// `true` iff the node is flagged as a hub. Unknown UUID → `false`.
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] for read failures.
    pub fn is_hub(&self, uuid: Uuid7) -> Result<bool, SwIndexError> {
        let raw = self.uuid_is_hub.get(uuid.as_bytes().as_slice())?;
        Ok(raw.is_some_and(|b| b.first().copied() == Some(1)))
    }

    /// The members of a cluster, or `None` if the cluster id is unknown.
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] for read failures;
    /// [`SwIndexError::Corruption`] for malformed stored data.
    pub fn cluster_members(&self, cluster_id: u32) -> Result<Option<Vec<Uuid7>>, SwIndexError> {
        let key = cluster_id.to_le_bytes();
        let raw = self.cluster_members.get(key.as_slice())?;
        raw.map(|b| decode_uuid_vec(&b, "cluster_members"))
            .transpose()
    }

    /// The hub-graph neighbors of a hub, or empty if the UUID isn't a
    /// hub (or doesn't exist).
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] for read failures;
    /// [`SwIndexError::Corruption`] for malformed stored data.
    pub fn hub_neighbors(&self, hub: Uuid7) -> Result<Vec<(Uuid7, f32)>, SwIndexError> {
        let raw = self.hub_neighbors.get(hub.as_bytes().as_slice())?;
        match raw {
            Some(b) => decode_hub_neighbors(&b, "hub_neighbors"),
            None => Ok(Vec::new()),
        }
    }

    /// Cheap introspection counts — node / cluster / hub totals.
    ///
    /// Uses Fjall's `approximate_len` per partition; values are
    /// accurate for a single-writer index built with one
    /// `build_from_source` call. After incremental updates land
    /// (issue #27) these become approximations.
    #[must_use]
    pub fn stats(&self) -> SwStats {
        SwStats {
            nodes: self.uuid_to_cluster.approximate_len(),
            clusters: self.cluster_members.approximate_len(),
            hubs: self.hub_neighbors.approximate_len(),
        }
    }

    /// Run a structured query against the persisted index.
    ///
    /// See [`QueryKind`] for the available operations. Returns a
    /// [`QueryResult`] with the matched uuids plus routing telemetry.
    ///
    /// # Errors
    ///
    /// * [`SwIndexError::Fjall`] for any underlying read failure.
    /// * [`SwIndexError::Corruption`] if stored data has the wrong
    ///   shape (cluster id stored as the wrong byte count, etc.).
    pub fn query(&self, query: QueryKind) -> Result<QueryResult, SwIndexError> {
        let _span = info_span!("swindex.query", kind = ?query).entered();
        let result = match query {
            QueryKind::SameCluster { start } => self.query_same_cluster(start),
            QueryKind::Similar { start, limit } => self.query_similar(start, limit),
        }?;
        info!(
            uuids = result.uuids.len(),
            clusters_visited = result.stats.clusters_visited,
            hubs_visited = result.stats.hubs_visited,
            "query complete"
        );
        Ok(result)
    }

    /// `QueryKind::SameCluster` — return every uuid in the same
    /// cluster as `start`. Pure Layer-1 lookup.
    fn query_same_cluster(&self, start: Uuid7) -> Result<QueryResult, SwIndexError> {
        // Step 1: which cluster is the seed in?
        let Some(cluster_id) = self.cluster_of(start)? else {
            // Unknown node — empty result, no clusters visited.
            return Ok(QueryResult {
                uuids: Vec::new(),
                stats: QueryStats::default(),
            });
        };
        // Step 2: fetch the cluster's members.
        let members = self.cluster_members(cluster_id)?.unwrap_or_default();
        Ok(QueryResult {
            uuids: members,
            stats: QueryStats {
                clusters_visited: 1,
                hubs_visited: 0,
            },
        })
    }

    /// `QueryKind::Similar` — same-cluster first, then expand to
    /// neighboring clusters via the hub graph. The textbook 4-layer
    /// routing for "find things related to X" queries.
    fn query_similar(&self, start: Uuid7, limit: usize) -> Result<QueryResult, SwIndexError> {
        if limit == 0 {
            return Ok(QueryResult {
                uuids: Vec::new(),
                stats: QueryStats::default(),
            });
        }

        // Step 1: resolve the start's cluster (Layer 1).
        let Some(start_cluster) = self.cluster_of(start)? else {
            return Ok(QueryResult {
                uuids: Vec::new(),
                stats: QueryStats::default(),
            });
        };

        // Track in-order accumulation and a dedup set in parallel.
        // The Vec preserves priority order; the BTreeSet prevents
        // duplicate entries when neighboring clusters overlap.
        let mut out: Vec<Uuid7> = Vec::with_capacity(limit);
        let mut seen: std::collections::BTreeSet<Uuid7> = std::collections::BTreeSet::new();
        seen.insert(start); // never include the seed itself in similar-results

        let mut clusters_visited = 0_usize;
        let mut hubs_visited = 0_usize;

        // Step 2 (priority 0): the start's own cluster members.
        let local = self.cluster_members(start_cluster)?.unwrap_or_default();
        clusters_visited += 1;
        for u in local {
            if out.len() >= limit {
                break;
            }
            if seen.insert(u) {
                out.push(u);
            }
        }

        if out.len() >= limit {
            return Ok(QueryResult {
                uuids: out,
                stats: QueryStats {
                    clusters_visited,
                    hubs_visited,
                },
            });
        }

        // Step 3: find a hub in the start's cluster to anchor the
        // hub-graph walk. Prefer the start itself if it's a hub;
        // otherwise scan the cluster members for any hub.
        let mut entry_hub: Option<Uuid7> = None;
        if self.is_hub(start)? {
            entry_hub = Some(start);
        } else {
            // Reload cluster members because the borrow above moved them.
            // (Cheap — same Fjall read, in OS cache.)
            for m in self.cluster_members(start_cluster)?.unwrap_or_default() {
                if self.is_hub(m)? {
                    entry_hub = Some(m);
                    break;
                }
            }
        }

        // If the start's cluster has no hub at all, we can't expand
        // further with what's persisted. Return what we have.
        let Some(entry) = entry_hub else {
            return Ok(QueryResult {
                uuids: out,
                stats: QueryStats {
                    clusters_visited,
                    hubs_visited,
                },
            });
        };

        // Step 4: walk the hub graph (Layer 2). Visit neighboring hubs
        // in descending weight order so closer clusters fill the
        // result first.
        let mut hub_edges = self.hub_neighbors(entry)?;
        hub_edges.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (neighbor_hub, _weight) in hub_edges {
            hubs_visited += 1;

            // Step 5: which cluster does this neighbor hub anchor?
            let Some(neighbor_cluster) = self.cluster_of(neighbor_hub)? else {
                continue;
            };
            if neighbor_cluster == start_cluster {
                continue; // already drained above
            }

            // Step 6: collect that cluster's members.
            let neighbor_members = self.cluster_members(neighbor_cluster)?.unwrap_or_default();
            clusters_visited += 1;
            for u in neighbor_members {
                if out.len() >= limit {
                    break;
                }
                if seen.insert(u) {
                    out.push(u);
                }
            }
            if out.len() >= limit {
                break;
            }
        }

        Ok(QueryResult {
            uuids: out,
            stats: QueryStats {
                clusters_visited,
                hubs_visited,
            },
        })
    }

    /// The (size, hub_count) pair for a cluster, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] for read failures;
    /// [`SwIndexError::Corruption`] for malformed stored data.
    pub fn cluster_meta(&self, cluster_id: u32) -> Result<Option<(u32, u32)>, SwIndexError> {
        let key = cluster_id.to_le_bytes();
        let raw = self.cluster_meta.get(key.as_slice())?;
        raw.map(|b| decode_cluster_meta(&b)).transpose()
    }

    // =====================================================================
    // Incremental maintenance (Phase 1 — issue #52)
    //
    // The scaffolding for issue #27 / Ada-IVF. `insert_node` appends to
    // existing structure and increments per-cluster drift counters.
    // `drift_report` reads them back. `maintain` runs whatever the
    // supplied [`MaintenancePolicy`] returns — `NeverRebalance` is a
    // no-op, so this is currently a stub call that does nothing useful
    // until Phase 2 ships `ThresholdRebalance`.
    // =====================================================================

    /// Drift state for one cluster. Returns `{generation: 0,
    /// delta_inserts: 0}` if the cluster has no recorded drift entry
    /// (e.g. an index built before Phase 1, or a cluster that hasn't
    /// been touched since the last rebuild).
    ///
    /// # Errors
    ///
    /// * [`SwIndexError::Fjall`] on a read failure.
    /// * [`SwIndexError::Corruption`] if the stored payload is malformed.
    /// * [`SwIndexError::UnsupportedFormat`] if the stored payload was
    ///   written by a newer swindex with a different format.
    pub fn cluster_drift(&self, cluster_id: u32) -> Result<(u64, u32), SwIndexError> {
        let key = cluster_id.to_le_bytes();
        let raw = self.cluster_drift.get(key.as_slice())?;
        match raw {
            Some(b) => decode_cluster_drift(&b),
            None => Ok((0, 0)),
        }
    }

    /// Read every cluster's drift state into a [`DriftReport`]. Walks
    /// the `cluster_drift` partition end-to-end; cost is linear in the
    /// number of clusters, which is small relative to the node count.
    ///
    /// # Errors
    ///
    /// * [`SwIndexError::Fjall`] on a read failure.
    /// * [`SwIndexError::Corruption`] / [`SwIndexError::UnsupportedFormat`]
    ///   if any payload is malformed.
    pub fn drift_report(&self) -> Result<DriftReport, SwIndexError> {
        let _span = info_span!("swindex.drift_report").entered();
        let mut per_cluster: BTreeMap<u32, ClusterDrift> = BTreeMap::new();
        for entry in self.cluster_drift.iter() {
            let (k, v) = entry?;
            if k.len() != 4 {
                return Err(SwIndexError::Corruption(format!(
                    "cluster_drift key expected 4 bytes, got {}",
                    k.len()
                )));
            }
            let mut id_bytes = [0u8; 4];
            id_bytes.copy_from_slice(&k);
            let cluster_id = u32::from_le_bytes(id_bytes);
            let (generation, delta_inserts) = decode_cluster_drift(&v)?;
            per_cluster.insert(
                cluster_id,
                ClusterDrift {
                    generation,
                    delta_inserts,
                },
            );
        }
        debug!(clusters = per_cluster.len(), "drift report assembled");
        Ok(DriftReport { per_cluster })
    }

    /// Append a node to the index, assigning it to an existing cluster
    /// via majority-vote of its seed neighbors (or to a new singleton
    /// cluster if none of the seeds are known). Returns the assigned
    /// cluster id.
    ///
    /// # Cluster assignment
    ///
    /// 1. Each seed neighbor's current cluster is looked up via
    ///    [`Self::cluster_of`]. Unknown seeds are silently skipped.
    /// 2. The cluster with the most votes wins. Ties are broken by
    ///    **lowest cluster id** so the operation is deterministic.
    /// 3. If no seed neighbor is known, a new singleton cluster is
    ///    allocated at `max(existing_cluster_ids) + 1` (or `0` for an
    ///    empty index). The singleton's region defaults to `0`.
    ///
    /// # What's updated atomically
    ///
    /// All six structural partitions plus `cluster_drift`, in a single
    /// Fjall batch followed by `PersistMode::SyncAll`:
    /// * `uuid_to_cluster`, `uuid_to_region`, `uuid_is_hub` (new node
    ///   defaults to non-hub)
    /// * `cluster_members` (member list rewritten with the new uuid
    ///   sorted in)
    /// * `cluster_meta` (size incremented; hub_count unchanged because
    ///   new nodes aren't hubs)
    /// * `cluster_drift` (delta_inserts incremented for the assigned
    ///   cluster)
    ///
    /// # What this does NOT do
    ///
    /// * **No re-clustering.** The new node lands in an existing
    ///   cluster verbatim; no Leiden pass runs. That's Phase 2+.
    /// * **No edge insertion.** Seed neighbors inform the cluster
    ///   choice but no persistent edge is recorded. Add-edge-between-
    ///   existing-nodes requires re-clustering and is out of scope.
    /// * **No hub re-detection.** The new node is flagged
    ///   `uuid_is_hub = false` unconditionally.
    /// * **No `region` adjustment.** The node inherits the cluster's
    ///   region (or 0 for a fresh singleton).
    ///
    /// # Errors
    ///
    /// * [`SwIndexError::Fjall`] on any read/write failure.
    /// * [`SwIndexError::Corruption`] if existing data is malformed.
    /// * [`SwIndexError::UnsupportedFormat`] if existing data has an
    ///   unrecognized format version.
    pub fn insert_node(
        &mut self,
        node: &Node,
        seed_neighbors: &[Uuid7],
    ) -> Result<u32, SwIndexError> {
        let _span =
            info_span!("swindex.insert_node", uuid = ?node.id, seeds = seed_neighbors.len())
                .entered();

        // Step 1: tally votes from known seed neighbors. BTreeMap so
        // iteration order is deterministic for tie-breaking.
        let mut votes: BTreeMap<u32, usize> = BTreeMap::new();
        for &seed in seed_neighbors {
            if let Some(c) = self.cluster_of(seed)? {
                *votes.entry(c).or_insert(0) += 1;
            }
        }

        // Step 2: pick winning cluster, or allocate a new singleton.
        let assigned: u32 = if votes.is_empty() {
            self.allocate_next_cluster_id()?
        } else {
            // Highest count wins; tie -> lowest cluster id.
            // BTreeMap iterates in ascending key order, so a stable
            // max_by_key on count alone gives the *last* (highest-id)
            // tie-breaker — we want the opposite, so we fold manually.
            let mut best_id = u32::MAX;
            let mut best_count = 0_usize;
            for (&cid, &count) in &votes {
                if count > best_count {
                    best_count = count;
                    best_id = cid;
                }
            }
            best_id
        };

        // Step 3: resolve region for the assigned cluster.
        // Existing cluster -> region of any current member. Singleton
        // -> region 0 (sensible default; no recursive Leiden runs).
        let assigned_region = self.cluster_region_or_default(assigned)?;

        // Step 4: build the atomic batch.
        let mut batch = self.keyspace.batch();

        batch.insert(
            &self.uuid_to_cluster,
            node.id.as_bytes().as_slice(),
            assigned.to_le_bytes().as_slice(),
        );
        batch.insert(
            &self.uuid_to_region,
            node.id.as_bytes().as_slice(),
            assigned_region.to_le_bytes().as_slice(),
        );
        batch.insert(
            &self.uuid_is_hub,
            node.id.as_bytes().as_slice(),
            [0u8].as_slice(),
        );

        // Update cluster_members: read, push, encode, write. The
        // member list is small relative to the full index so the
        // re-encode cost is bounded by cluster size, not graph size.
        let mut members = self.cluster_members(assigned)?.unwrap_or_default();
        // Maintain sorted order — `build_from_source` produces members
        // in cluster-internal order; `insert_node` is the new write
        // path and sorting keeps `cluster_members(c)` output stable
        // across rebuild vs. insert.
        let new_uuid = node.id;
        match members.binary_search(&new_uuid) {
            Ok(_) => {
                // Already present — `insert_node` on an existing uuid
                // is a contract violation. Bail without mutating
                // anything.
                return Err(SwIndexError::Corruption(format!(
                    "insert_node: uuid {} already exists in the index",
                    new_uuid.as_uuid()
                )));
            }
            Err(pos) => members.insert(pos, new_uuid),
        }
        let members_buf = encode_uuid_vec(&members)?;
        batch.insert(
            &self.cluster_members,
            assigned.to_le_bytes().as_slice(),
            members_buf,
        );

        // Update cluster_meta: size += 1; hub_count unchanged (new
        // nodes aren't hubs in Phase 1).
        let (existing_size, existing_hub_count) = self.cluster_meta(assigned)?.unwrap_or((0, 0));
        let new_size = existing_size
            .checked_add(1)
            .ok_or_else(|| SwIndexError::Corruption("cluster size overflow on insert".into()))?;
        batch.insert(
            &self.cluster_meta,
            assigned.to_le_bytes().as_slice(),
            encode_cluster_meta(new_size, existing_hub_count),
        );

        // Update cluster_drift: delta_inserts += 1; generation unchanged.
        let (generation, delta) = self.cluster_drift(assigned)?;
        let new_delta = delta
            .checked_add(1)
            .ok_or_else(|| SwIndexError::Corruption("cluster_drift overflow on insert".into()))?;
        batch.insert(
            &self.cluster_drift,
            assigned.to_le_bytes().as_slice(),
            encode_cluster_drift(generation, new_delta),
        );

        batch.commit()?;
        self.keyspace.persist(PersistMode::SyncAll)?;
        info!(
            assigned_cluster = assigned,
            assigned_region, new_size, "insert_node committed"
        );
        Ok(assigned)
    }

    /// Ask the [`MaintenancePolicy`] what (if anything) to do and
    /// apply its decisions. Phase 1 ships [`NeverRebalance`] which
    /// always returns an empty action list — so this is a no-op until
    /// Phase 2 lands real policies.
    ///
    /// Returning a [`MaintenanceReport`] even for no-op calls means
    /// downstream observability (logs, metrics) gets a consistent
    /// shape regardless of policy.
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] if reading drift state fails. Today
    /// nothing else can fail; future variants of [`MaintenanceAction`]
    /// will surface their own errors via this return.
    pub fn maintain<P: MaintenancePolicy>(
        &mut self,
        policy: &P,
    ) -> Result<MaintenanceReport, SwIndexError> {
        let _span = info_span!("swindex.maintain").entered();
        let drift = self.drift_report()?;
        let actions = policy.decide(&drift);
        // Phase 1: every variant of MaintenanceAction is a no-op, so
        // we just thread them into the report. Phase 2 will dispatch
        // on the variant here.
        debug!(actions = actions.len(), "policy returned actions");
        Ok(MaintenanceReport {
            actions_taken: actions,
        })
    }

    /// Find the next free cluster id by scanning existing cluster
    /// metadata for the current max. Cost is linear in the cluster
    /// count, which is small (typically `< 100`) relative to the
    /// node count.
    ///
    /// Returns `0` for an empty index.
    fn allocate_next_cluster_id(&self) -> Result<u32, SwIndexError> {
        let mut max_id: Option<u32> = None;
        for entry in self.cluster_meta.iter() {
            let (k, _v) = entry?;
            if k.len() != 4 {
                return Err(SwIndexError::Corruption(format!(
                    "cluster_meta key expected 4 bytes, got {}",
                    k.len()
                )));
            }
            let mut id_bytes = [0u8; 4];
            id_bytes.copy_from_slice(&k);
            let cluster_id = u32::from_le_bytes(id_bytes);
            max_id = Some(max_id.map_or(cluster_id, |m| m.max(cluster_id)));
        }
        Ok(match max_id {
            Some(m) => m
                .checked_add(1)
                .ok_or_else(|| SwIndexError::Corruption("cluster id space exhausted".into()))?,
            None => 0,
        })
    }

    /// Region id for a cluster, derived from any current member's
    /// region. Returns `0` for an empty / nonexistent cluster (the
    /// singleton-allocation default).
    fn cluster_region_or_default(&self, cluster_id: u32) -> Result<u32, SwIndexError> {
        if let Some(members) = self.cluster_members(cluster_id)? {
            if let Some(&first_member) = members.first() {
                return Ok(self.region_of(first_member)?.unwrap_or(0));
            }
        }
        Ok(0)
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for SwIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Compact summary — we don't dump the keyspace contents in
        // panic output; that would be enormous.
        f.debug_struct("SwIndex")
            .field("partitions", &6_usize)
            .finish()
    }
}

// =========================================================================
// Encoding helpers
// =========================================================================

/// Strip the leading format-version byte from a payload and return the
/// remainder. Rejects unknown versions with
/// [`SwIndexError::UnsupportedFormat`]. Centralizing this check means
/// the three decoders below stay focused on their own layout and we
/// only have one place to teach about a future v2.
fn read_format_byte<'a>(bytes: &'a [u8], context: &'static str) -> Result<&'a [u8], SwIndexError> {
    let Some((&version, rest)) = bytes.split_first() else {
        return Err(SwIndexError::Corruption(format!(
            "{context}: payload is empty (expected at least 1 byte for format version)"
        )));
    };
    if version != FORMAT_V1 {
        return Err(SwIndexError::UnsupportedFormat {
            found: version,
            context,
        });
    }
    Ok(rest)
}

fn encode_uuid_vec(uuids: &[Uuid7]) -> Result<Vec<u8>, SwIndexError> {
    // 1-byte format version + 4-byte length prefix (u32 LE) + 16 B per uuid.
    let len = u32::try_from(uuids.len())
        .map_err(|_| SwIndexError::Corruption("vec len exceeds u32".into()))?;
    let mut buf = Vec::with_capacity(1 + 4 + uuids.len() * 16);
    buf.push(FORMAT_V1);
    buf.extend_from_slice(&len.to_le_bytes());
    for u in uuids {
        buf.extend_from_slice(u.as_bytes());
    }
    Ok(buf)
}

fn decode_uuid_vec(bytes: &[u8], context: &'static str) -> Result<Vec<Uuid7>, SwIndexError> {
    let body = read_format_byte(bytes, context)?;
    if body.len() < 4 {
        return Err(SwIndexError::Corruption(format!(
            "{context}: payload body < 4 bytes after version byte"
        )));
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&body[0..4]);
    let len = u32::from_le_bytes(len_bytes) as usize;
    let expected = 4 + len * 16;
    if body.len() != expected {
        return Err(SwIndexError::Corruption(format!(
            "{context}: expected {expected} body bytes for len={len}, got {}",
            body.len()
        )));
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let start = 4 + i * 16;
        let end = start + 16;
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&body[start..end]);
        let raw = Uuid::from_bytes(buf);
        let wrapped = Uuid7::from_uuid(raw).ok_or_else(|| {
            SwIndexError::Corruption(format!(
                "{context}: stored UUID at body offset {start} is not version 7"
            ))
        })?;
        out.push(wrapped);
    }
    Ok(out)
}

fn encode_hub_neighbors(items: &[(Uuid7, f32)]) -> Result<Vec<u8>, SwIndexError> {
    // 1-byte format version + 4-byte length prefix + 20 B per item.
    let len = u32::try_from(items.len())
        .map_err(|_| SwIndexError::Corruption("hub-neighbors len exceeds u32".into()))?;
    let mut buf = Vec::with_capacity(1 + 4 + items.len() * 20);
    buf.push(FORMAT_V1);
    buf.extend_from_slice(&len.to_le_bytes());
    for (uuid, w) in items {
        buf.extend_from_slice(uuid.as_bytes());
        buf.extend_from_slice(&w.to_le_bytes());
    }
    Ok(buf)
}

fn decode_hub_neighbors(
    bytes: &[u8],
    context: &'static str,
) -> Result<Vec<(Uuid7, f32)>, SwIndexError> {
    let body = read_format_byte(bytes, context)?;
    if body.len() < 4 {
        return Err(SwIndexError::Corruption(format!(
            "{context}: payload body < 4 bytes after version byte"
        )));
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&body[0..4]);
    let len = u32::from_le_bytes(len_bytes) as usize;
    let expected = 4 + len * 20;
    if body.len() != expected {
        return Err(SwIndexError::Corruption(format!(
            "{context}: expected {expected} body bytes for len={len}, got {}",
            body.len()
        )));
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let base = 4 + i * 20;
        let mut uuid_buf = [0u8; 16];
        uuid_buf.copy_from_slice(&body[base..base + 16]);
        let raw = Uuid::from_bytes(uuid_buf);
        let wrapped = Uuid7::from_uuid(raw).ok_or_else(|| {
            SwIndexError::Corruption(format!(
                "{context}: stored hub neighbor at body offset {base} is not v7"
            ))
        })?;
        let mut w_buf = [0u8; 4];
        w_buf.copy_from_slice(&body[base + 16..base + 20]);
        out.push((wrapped, f32::from_le_bytes(w_buf)));
    }
    Ok(out)
}

fn encode_cluster_meta(size: u32, hub_count: u32) -> Vec<u8> {
    // 1-byte format version + 4-byte size LE + 4-byte hub_count LE = 9 B.
    let mut buf = Vec::with_capacity(9);
    buf.push(FORMAT_V1);
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&hub_count.to_le_bytes());
    buf
}

fn encode_cluster_drift(generation: u64, delta_inserts: u32) -> Vec<u8> {
    // 1-byte format version + 8-byte generation LE + 4-byte delta_inserts LE = 13 B.
    let mut buf = Vec::with_capacity(13);
    buf.push(FORMAT_V1);
    buf.extend_from_slice(&generation.to_le_bytes());
    buf.extend_from_slice(&delta_inserts.to_le_bytes());
    buf
}

fn decode_cluster_drift(bytes: &[u8]) -> Result<(u64, u32), SwIndexError> {
    let body = read_format_byte(bytes, "cluster_drift")?;
    if body.len() != 12 {
        return Err(SwIndexError::Corruption(format!(
            "cluster_drift expected 12 body bytes after version, got {}",
            body.len()
        )));
    }
    let mut gen_bytes = [0u8; 8];
    gen_bytes.copy_from_slice(&body[0..8]);
    let mut delta_bytes = [0u8; 4];
    delta_bytes.copy_from_slice(&body[8..12]);
    Ok((
        u64::from_le_bytes(gen_bytes),
        u32::from_le_bytes(delta_bytes),
    ))
}

fn decode_cluster_meta(bytes: &[u8]) -> Result<(u32, u32), SwIndexError> {
    let body = read_format_byte(bytes, "cluster_meta")?;
    if body.len() != 8 {
        return Err(SwIndexError::Corruption(format!(
            "cluster_meta expected 8 body bytes after version, got {}",
            body.len()
        )));
    }
    let mut size_bytes = [0u8; 4];
    size_bytes.copy_from_slice(&body[0..4]);
    let mut hub_bytes = [0u8; 4];
    hub_bytes.copy_from_slice(&body[4..8]);
    Ok((
        u32::from_le_bytes(size_bytes),
        u32::from_le_bytes(hub_bytes),
    ))
}

fn decode_u32(bytes: &[u8], context: &str) -> Result<u32, SwIndexError> {
    if bytes.len() != 4 {
        return Err(SwIndexError::Corruption(format!(
            "{context}: expected 4 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(bytes);
    Ok(u32::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::{
        BuildStats, FORMAT_V1, QueryKind, SwIndex, SwIndexError, decode_cluster_meta,
        decode_hub_neighbors, decode_uuid_vec, encode_cluster_meta, encode_hub_neighbors,
        encode_uuid_vec,
    };
    use crate::id::Uuid7;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::{GraphSource, SliceSource};
    use tempfile::TempDir;

    fn two_disjoint_triangles() -> (Vec<Node>, Vec<Edge>) {
        let mk = |k: &str| Node::fresh(NodeKind::new(k));
        let a1 = mk("v");
        let b1 = mk("v");
        let c1 = mk("v");
        let a2 = mk("v");
        let b2 = mk("v");
        let c2 = mk("v");
        let edges = vec![
            Edge::fresh(a1.id, b1.id, EdgeKind::new("e")),
            Edge::fresh(b1.id, c1.id, EdgeKind::new("e")),
            Edge::fresh(c1.id, a1.id, EdgeKind::new("e")),
            Edge::fresh(a2.id, b2.id, EdgeKind::new("e")),
            Edge::fresh(b2.id, c2.id, EdgeKind::new("e")),
            Edge::fresh(c2.id, a2.id, EdgeKind::new("e")),
        ];
        (vec![a1, b1, c1, a2, b2, c2], edges)
    }

    #[test]
    fn open_creates_keyspace_directory() {
        let dir = TempDir::new().unwrap();
        // The directory exists already (TempDir creates it); we just
        // need to confirm SwIndex::open succeeds against it without
        // panicking.
        let _idx = SwIndex::open(dir.path()).unwrap();
    }

    #[test]
    fn build_populates_all_six_partitions_and_returns_stats() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);

        let stats = idx.build_from_source(&src).unwrap();
        assert_eq!(
            stats,
            BuildStats {
                nodes: 6,
                clusters: 2,
                regions: 2,
                hubs: 1, // 10% of 6 ceil = 1
            }
        );

        // Spot-check every read accessor: the first triangle's nodes
        // share a cluster; the second triangle's nodes share another.
        let cluster_a = idx.cluster_of(nodes[0].id).unwrap().unwrap();
        let cluster_b = idx.cluster_of(nodes[3].id).unwrap().unwrap();
        assert_eq!(idx.cluster_of(nodes[1].id).unwrap().unwrap(), cluster_a);
        assert_eq!(idx.cluster_of(nodes[2].id).unwrap().unwrap(), cluster_a);
        assert_eq!(idx.cluster_of(nodes[4].id).unwrap().unwrap(), cluster_b);
        assert_eq!(idx.cluster_of(nodes[5].id).unwrap().unwrap(), cluster_b);
        assert_ne!(cluster_a, cluster_b);

        // cluster_members for cluster_a must contain exactly the first
        // triangle's three UUIDs (sorted by Uuid7).
        let members_a = idx.cluster_members(cluster_a).unwrap().unwrap();
        assert_eq!(members_a.len(), 3);
        let expected_a: std::collections::BTreeSet<_> = [nodes[0].id, nodes[1].id, nodes[2].id]
            .into_iter()
            .collect();
        let got_a: std::collections::BTreeSet<_> = members_a.into_iter().collect();
        assert_eq!(got_a, expected_a);

        // cluster_meta: size = 3, hub_count = 0 or 1 (depends which
        // triangle won the single hub slot).
        let (size_a, hub_count_a) = idx.cluster_meta(cluster_a).unwrap().unwrap();
        assert_eq!(size_a, 3);
        assert!(hub_count_a <= 1);

        // Region check: nodes in the same cluster share a region. With
        // disjoint triangles each cluster is its own region (no
        // inter-cluster edges to merge).
        let region_a = idx.region_of(nodes[0].id).unwrap().unwrap();
        let region_b = idx.region_of(nodes[3].id).unwrap().unwrap();
        assert_ne!(region_a, region_b);

        // is_hub: exactly one node total is a hub.
        let total_hubs: usize = nodes
            .iter()
            .map(|n| usize::from(idx.is_hub(n.id).unwrap()))
            .sum();
        assert_eq!(total_hubs, 1);
    }

    #[test]
    fn round_trip_via_close_and_reopen() {
        // The persistence invariant: close + reopen returns identical
        // query results. Without this, swindex is just an in-memory
        // structure with a misleading API.
        let dir = TempDir::new().unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);

        let original_stats;
        let original_cluster_0;
        let original_region_0;
        {
            let mut idx = SwIndex::open(dir.path()).unwrap();
            original_stats = idx.build_from_source(&src).unwrap();
            original_cluster_0 = idx.cluster_of(nodes[0].id).unwrap().unwrap();
            original_region_0 = idx.region_of(nodes[0].id).unwrap().unwrap();
            // drop here closes the keyspace
        }

        // Reopen at the same path and verify everything is still there.
        let reopened = SwIndex::open(dir.path()).unwrap();
        assert_eq!(
            reopened.cluster_of(nodes[0].id).unwrap().unwrap(),
            original_cluster_0
        );
        assert_eq!(
            reopened.region_of(nodes[0].id).unwrap().unwrap(),
            original_region_0
        );
        // Every node still queryable.
        for n in &nodes {
            assert!(reopened.cluster_of(n.id).unwrap().is_some());
            assert!(reopened.region_of(n.id).unwrap().is_some());
        }
        // Cluster member listings still readable.
        let members = reopened
            .cluster_members(original_cluster_0)
            .unwrap()
            .unwrap();
        assert_eq!(members.len(), 3);
        // Stats recomputable via partition contents (we didn't store
        // them, but every node-count is recoverable from the partitions).
        let _ = original_stats;
    }

    #[test]
    fn unknown_uuid_returns_none() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        let random = crate::id::Uuid7::now();
        assert!(idx.cluster_of(random).unwrap().is_none());
        assert!(idx.region_of(random).unwrap().is_none());
        assert!(!idx.is_hub(random).unwrap());
        assert_eq!(idx.hub_neighbors(random).unwrap(), Vec::new());
    }

    /// Headline test: build on Zachary, query every node, then reopen
    /// and re-query. Every cluster_of/region_of/is_hub answer must
    /// match across the close+reopen boundary.
    #[test]
    fn zachary_build_and_round_trip() {
        let dir = TempDir::new().unwrap();
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();

        let mut idx = SwIndex::open(dir.path()).unwrap();
        let stats = idx.build_from_source(&src).unwrap();
        assert_eq!(stats.nodes, 34);
        assert_eq!(stats.clusters, 4);
        // Top 10% of 34 ceil = 4 hubs.
        assert_eq!(stats.hubs, 4);

        // Snapshot every per-node answer.
        let original_answers: Vec<(u32, u32, bool)> = src
            .nodes()
            .map(|n| {
                (
                    idx.cluster_of(n.id).unwrap().unwrap(),
                    idx.region_of(n.id).unwrap().unwrap(),
                    idx.is_hub(n.id).unwrap(),
                )
            })
            .collect();

        // Close + reopen.
        drop(idx);
        let reopened = SwIndex::open(dir.path()).unwrap();
        let reopened_answers: Vec<(u32, u32, bool)> = src
            .nodes()
            .map(|n| {
                (
                    reopened.cluster_of(n.id).unwrap().unwrap(),
                    reopened.region_of(n.id).unwrap().unwrap(),
                    reopened.is_hub(n.id).unwrap(),
                )
            })
            .collect();

        assert_eq!(original_answers, reopened_answers);

        // 4 clusters present.
        let clusters_u32 = u32::try_from(stats.clusters).expect("cluster count fits in u32");
        for cid in 0..clusters_u32 {
            let members = reopened.cluster_members(cid).unwrap().unwrap();
            assert!(!members.is_empty());
            let (size, hubs) = reopened.cluster_meta(cid).unwrap().unwrap();
            assert_eq!(size as usize, members.len());
            assert!(hubs <= size);
        }
    }

    // =====================================================================
    // Query planner tests
    // =====================================================================

    #[test]
    fn stats_match_build_counts() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let build_stats = idx.build_from_source(&src).unwrap();

        let s = idx.stats();
        assert_eq!(s.nodes, build_stats.nodes);
        assert_eq!(s.clusters, build_stats.clusters);
        assert_eq!(s.hubs, build_stats.hubs);
    }

    #[test]
    fn same_cluster_returns_full_cluster() {
        // On two disjoint triangles: SameCluster on a triangle-member
        // returns exactly that triangle's 3 nodes.
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        let result = idx
            .query(QueryKind::SameCluster { start: nodes[0].id })
            .unwrap();
        assert_eq!(result.uuids.len(), 3);
        // The 3 must be exactly {nodes[0], nodes[1], nodes[2]} as a set.
        let got: std::collections::BTreeSet<_> = result.uuids.iter().copied().collect();
        let want: std::collections::BTreeSet<_> = [nodes[0].id, nodes[1].id, nodes[2].id]
            .into_iter()
            .collect();
        assert_eq!(got, want);
        // One cluster visited, no hub-graph traversal needed.
        assert_eq!(result.stats.clusters_visited, 1);
        assert_eq!(result.stats.hubs_visited, 0);
    }

    #[test]
    fn same_cluster_on_unknown_uuid_returns_empty() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        let unknown = crate::id::Uuid7::now();
        let result = idx
            .query(QueryKind::SameCluster { start: unknown })
            .unwrap();
        assert!(result.uuids.is_empty());
        assert_eq!(result.stats.clusters_visited, 0);
    }

    #[test]
    fn similar_with_limit_zero_returns_empty() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        let result = idx
            .query(QueryKind::Similar {
                start: nodes[0].id,
                limit: 0,
            })
            .unwrap();
        assert!(result.uuids.is_empty());
    }

    #[test]
    fn similar_respects_limit() {
        // Two disjoint triangles, no inter-cluster edges, so the hub
        // graph has no edges across them. Similar must return only
        // same-cluster members (at most 2 others), bounded by limit.
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        let result = idx
            .query(QueryKind::Similar {
                start: nodes[0].id,
                limit: 1,
            })
            .unwrap();
        assert!(result.uuids.len() <= 1);
        // The seed itself must not be in the results.
        assert!(!result.uuids.contains(&nodes[0].id));
    }

    /// Headline test: on Zachary, `Similar(Mr Hi, 33)` must return up
    /// to 33 other nodes — all of them, since limit ≥ 33. The first
    /// chunk is Mr Hi's cluster; the rest come via the hub graph from
    /// other clusters' members.
    #[test]
    fn similar_on_zachary_fans_out_via_hub_graph() {
        let dir = TempDir::new().unwrap();
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();

        let mut idx = SwIndex::open(dir.path()).unwrap();
        idx.build_from_source(&src).unwrap();

        // Mr Hi is the first node minted by the GML loader (gml id 1).
        // We don't know his exact Uuid7 here but any node from src
        // works for this test.
        let some_node = src.nodes().next().unwrap();

        // Request many more than one cluster's worth — forces the hub-
        // graph expansion path to kick in.
        let result = idx
            .query(QueryKind::Similar {
                start: some_node.id,
                limit: 33,
            })
            .unwrap();

        // Should pull in nodes from multiple clusters.
        assert!(
            result.stats.clusters_visited >= 2,
            "expected hub-graph expansion to touch >= 2 clusters, got {}",
            result.stats.clusters_visited
        );
        assert!(
            result.stats.hubs_visited > 0,
            "expected the hub-graph to be walked"
        );

        // Result should not include the seed.
        assert!(!result.uuids.contains(&some_node.id));

        // Result should not exceed limit and should be deduplicated.
        assert!(result.uuids.len() <= 33);
        let uniq: std::collections::BTreeSet<_> = result.uuids.iter().copied().collect();
        assert_eq!(uniq.len(), result.uuids.len(), "result has duplicates");
    }

    // =====================================================================
    // Format-version-byte tests (issue #49)
    //
    // These tests pin the contract that all variable-length encodings
    // begin with `FORMAT_V1` and that decoders refuse anything else with
    // `SwIndexError::UnsupportedFormat` (rather than silently misreading
    // a future format as corrupted data — that distinction matters for
    // operator messaging).
    // =====================================================================

    #[test]
    fn format_version_byte_is_first_in_every_variable_payload() {
        // Encoders are the single source of truth for the on-disk shape.
        // If a future change forgets the version byte, this test fires
        // before round-trip tests do.
        let uuids = vec![Uuid7::now(), Uuid7::now()];
        let buf = encode_uuid_vec(&uuids).unwrap();
        assert_eq!(buf.first(), Some(&FORMAT_V1), "uuid_vec missing version");

        let neighbors = vec![(Uuid7::now(), 0.5_f32)];
        let buf = encode_hub_neighbors(&neighbors).unwrap();
        assert_eq!(
            buf.first(),
            Some(&FORMAT_V1),
            "hub_neighbors missing version"
        );

        let buf = encode_cluster_meta(42, 7);
        assert_eq!(
            buf.first(),
            Some(&FORMAT_V1),
            "cluster_meta missing version"
        );
    }

    #[test]
    fn decode_rejects_future_version_with_unsupported_format() {
        // Hand-craft a buffer with a *future* version byte (V1 + 1) and
        // confirm the decoder refuses it instead of returning garbage.
        // This is the load-bearing forward-compat test from issue #49 §4.
        let mut buf = encode_uuid_vec(&[Uuid7::now()]).unwrap();
        buf[0] = FORMAT_V1 + 1;
        match decode_uuid_vec(&buf, "cluster_members") {
            Err(SwIndexError::UnsupportedFormat { found, context }) => {
                assert_eq!(found, FORMAT_V1 + 1);
                assert_eq!(context, "cluster_members");
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }

        let mut buf = encode_hub_neighbors(&[(Uuid7::now(), 1.0)]).unwrap();
        buf[0] = 0xFF;
        match decode_hub_neighbors(&buf, "hub_neighbors") {
            Err(SwIndexError::UnsupportedFormat { found, context }) => {
                assert_eq!(found, 0xFF);
                assert_eq!(context, "hub_neighbors");
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }

        let mut buf = encode_cluster_meta(1, 0);
        buf[0] = 0x00;
        match decode_cluster_meta(&buf) {
            Err(SwIndexError::UnsupportedFormat { found, context }) => {
                assert_eq!(found, 0x00);
                assert_eq!(context, "cluster_meta");
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_empty_payload_as_corruption() {
        // Empty payload is corruption (not a version mismatch). We don't
        // want to conflate the two — operators read these errors and
        // decide whether to upgrade swindex (UnsupportedFormat) or
        // rebuild from source (Corruption).
        match decode_uuid_vec(&[], "cluster_members") {
            Err(SwIndexError::Corruption(_)) => {}
            other => panic!("expected Corruption for empty payload, got {other:?}"),
        }
    }

    // =====================================================================
    // Phase 1 (issue #52): insert_node / drift_report / maintain
    // =====================================================================

    #[test]
    fn insert_node_with_known_neighbors_assigns_majority_cluster() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        // The first triangle's nodes (0,1,2) share a cluster. A new
        // node whose seed neighbors are nodes[0] and nodes[1] should
        // land in their cluster.
        let cluster_a = idx.cluster_of(nodes[0].id).unwrap().unwrap();
        let new_node = crate::node::Node::fresh(NodeKind::new("v"));
        let new_uuid = new_node.id;
        let assigned = idx
            .insert_node(&new_node, &[nodes[0].id, nodes[1].id])
            .unwrap();

        assert_eq!(
            assigned, cluster_a,
            "majority vote should land in cluster_a"
        );

        // Inserted node is now queryable.
        assert_eq!(idx.cluster_of(new_uuid).unwrap(), Some(cluster_a));
        assert!(
            !idx.is_hub(new_uuid).unwrap(),
            "new nodes default to non-hub"
        );

        // cluster_members now includes the new uuid.
        let members = idx.cluster_members(cluster_a).unwrap().unwrap();
        assert!(
            members.contains(&new_uuid),
            "new uuid missing from cluster_members"
        );
        assert_eq!(members.len(), 4, "cluster_a grew from 3 to 4");

        // cluster_meta size bumped by 1; hub_count unchanged.
        let (size, _hub_count) = idx.cluster_meta(cluster_a).unwrap().unwrap();
        assert_eq!(size, 4);

        // cluster_drift recorded the insert.
        let (gen_, delta) = idx.cluster_drift(cluster_a).unwrap();
        assert_eq!(gen_, 0, "build set generation to 0");
        assert_eq!(delta, 1, "exactly one insert into cluster_a");
    }

    #[test]
    fn insert_node_tie_breaks_to_lowest_cluster_id() {
        // One seed from cluster_a, one from cluster_b -> tie -> lower id wins.
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        let cluster_a = idx.cluster_of(nodes[0].id).unwrap().unwrap();
        let cluster_b = idx.cluster_of(nodes[3].id).unwrap().unwrap();
        let lower = cluster_a.min(cluster_b);
        let one_from_a = nodes[0].id;
        let one_from_b = nodes[3].id;

        let new_node = crate::node::Node::fresh(NodeKind::new("v"));
        let assigned = idx
            .insert_node(&new_node, &[one_from_a, one_from_b])
            .unwrap();
        assert_eq!(assigned, lower, "tie should resolve to lower cluster id");
    }

    #[test]
    fn insert_node_with_no_known_seeds_creates_singleton() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let build = idx.build_from_source(&src).unwrap();

        // Seed neighbors are all unknown uuids -> no votes -> singleton.
        let phantom_a = crate::id::Uuid7::now();
        let phantom_b = crate::id::Uuid7::now();
        let new_node = crate::node::Node::fresh(NodeKind::new("v"));
        let new_uuid = new_node.id;
        let assigned = idx.insert_node(&new_node, &[phantom_a, phantom_b]).unwrap();

        // New cluster id should be past the build's cluster count.
        assert!(
            assigned as usize >= build.clusters,
            "singleton {assigned} should be at or past existing cluster count {}",
            build.clusters
        );

        // The new cluster has exactly one member: the inserted uuid.
        let members = idx.cluster_members(assigned).unwrap().unwrap();
        assert_eq!(members, vec![new_uuid]);

        // cluster_meta size = 1; cluster_drift delta = 1.
        let (size, hub_count) = idx.cluster_meta(assigned).unwrap().unwrap();
        assert_eq!((size, hub_count), (1, 0));
        let (_gen, delta) = idx.cluster_drift(assigned).unwrap();
        assert_eq!(delta, 1);
    }

    #[test]
    fn insert_node_with_empty_seeds_creates_singleton() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let build = idx.build_from_source(&src).unwrap();

        let new_node = crate::node::Node::fresh(NodeKind::new("v"));
        let assigned = idx.insert_node(&new_node, &[]).unwrap();
        assert!(assigned as usize >= build.clusters);
    }

    #[test]
    fn insert_node_rejects_duplicate_uuid() {
        // Inserting a uuid that already exists is a contract violation
        // — Phase 1 surfaces it as a Corruption error rather than
        // silently double-counting drift.
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        // Re-insert one of the original nodes — same uuid.
        let dup = crate::node::Node {
            id: nodes[0].id,
            kind: NodeKind::new("v"),
        };
        let err = idx.insert_node(&dup, &[nodes[1].id]).unwrap_err();
        match err {
            SwIndexError::Corruption(msg) => assert!(
                msg.contains("already exists"),
                "expected duplicate-uuid corruption, got {msg}"
            ),
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[test]
    fn drift_report_after_inserts_matches_individual_lookups() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let build = idx.build_from_source(&src).unwrap();

        let cluster_a = idx.cluster_of(nodes[0].id).unwrap().unwrap();
        let cluster_b = idx.cluster_of(nodes[3].id).unwrap().unwrap();

        // 2 inserts into cluster_a, 1 into cluster_b.
        for _ in 0..2 {
            let n = crate::node::Node::fresh(NodeKind::new("v"));
            idx.insert_node(&n, &[nodes[0].id]).unwrap();
        }
        let n = crate::node::Node::fresh(NodeKind::new("v"));
        idx.insert_node(&n, &[nodes[3].id]).unwrap();

        let report = idx.drift_report().unwrap();
        assert_eq!(
            report.cluster_count(),
            build.clusters,
            "drift report should cover every cluster from the build"
        );
        assert_eq!(report.total_inserts(), 3);
        assert_eq!(report.per_cluster.get(&cluster_a).unwrap().delta_inserts, 2);
        assert_eq!(report.per_cluster.get(&cluster_b).unwrap().delta_inserts, 1);

        // Cross-check: drift_report and cluster_drift agree per cluster.
        for (&cid, cd) in &report.per_cluster {
            let (gen_, delta) = idx.cluster_drift(cid).unwrap();
            assert_eq!(cd.generation, gen_);
            assert_eq!(cd.delta_inserts, delta);
        }
    }

    #[test]
    fn maintain_with_never_rebalance_is_a_noop() {
        let dir = TempDir::new().unwrap();
        let mut idx = SwIndex::open(dir.path()).unwrap();
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        idx.build_from_source(&src).unwrap();

        // Generate some drift so the policy has something to look at.
        for _ in 0..5 {
            let n = crate::node::Node::fresh(NodeKind::new("v"));
            idx.insert_node(&n, &[nodes[0].id]).unwrap();
        }

        // Snapshot state before maintain.
        let drift_before = idx.drift_report().unwrap();
        let cluster_a = idx.cluster_of(nodes[0].id).unwrap().unwrap();
        let (size_before, _) = idx.cluster_meta(cluster_a).unwrap().unwrap();

        let report = idx.maintain(&crate::maintenance::NeverRebalance).unwrap();
        assert!(
            report.actions_taken.is_empty(),
            "NeverRebalance should return no actions"
        );

        // Nothing changed.
        let drift_after = idx.drift_report().unwrap();
        assert_eq!(
            drift_after.total_inserts(),
            drift_before.total_inserts(),
            "maintain with NeverRebalance must not reset drift"
        );
        let (size_after, _) = idx.cluster_meta(cluster_a).unwrap().unwrap();
        assert_eq!(size_after, size_before);
    }

    #[test]
    fn insert_node_survives_close_and_reopen() {
        // Persistence invariant for the new write path: insert, close,
        // reopen, confirm the insert stuck.
        let dir = TempDir::new().unwrap();
        let new_uuid;
        let assigned_cluster;
        {
            let mut idx = SwIndex::open(dir.path()).unwrap();
            let (nodes, edges) = two_disjoint_triangles();
            let src = SliceSource::new(&nodes, &edges);
            idx.build_from_source(&src).unwrap();
            let new_node = crate::node::Node::fresh(NodeKind::new("v"));
            new_uuid = new_node.id;
            assigned_cluster = idx.insert_node(&new_node, &[nodes[0].id]).unwrap();
        }

        let reopened = SwIndex::open(dir.path()).unwrap();
        assert_eq!(
            reopened.cluster_of(new_uuid).unwrap(),
            Some(assigned_cluster)
        );
        let (_gen, delta) = reopened.cluster_drift(assigned_cluster).unwrap();
        assert_eq!(delta, 1, "drift counter must survive close/reopen");

        // Query still works post-insert.
        let result = reopened
            .query(QueryKind::SameCluster { start: new_uuid })
            .unwrap();
        assert!(result.uuids.contains(&new_uuid));
    }

    #[test]
    fn round_trip_each_variable_encoder() {
        // Pure encoding round-trips, isolated from Fjall, so a regression
        // in the byte layout shows up here before the persistence tests.
        let uuids = vec![Uuid7::now(), Uuid7::now(), Uuid7::now()];
        let buf = encode_uuid_vec(&uuids).unwrap();
        let decoded = decode_uuid_vec(&buf, "cluster_members").unwrap();
        assert_eq!(decoded, uuids);

        let neighbors = vec![(Uuid7::now(), 0.25_f32), (Uuid7::now(), 0.5_f32)];
        let buf = encode_hub_neighbors(&neighbors).unwrap();
        let decoded = decode_hub_neighbors(&buf, "hub_neighbors").unwrap();
        assert_eq!(decoded.len(), neighbors.len());
        for (got, want) in decoded.iter().zip(neighbors.iter()) {
            assert_eq!(got.0, want.0);
            assert!((got.1 - want.1).abs() < f32::EPSILON);
        }

        let buf = encode_cluster_meta(123, 4);
        assert_eq!(decode_cluster_meta(&buf).unwrap(), (123, 4));
    }
}
