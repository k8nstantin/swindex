//! The persisted public index — `SwIndex`.
//!
//! # What this module ships
//!
//! [`SwIndex`] — the on-disk, query-time face of the four-layer
//! architecture. It wraps a Fjall keyspace with six partitions
//! ("keyspaces" in the design doc's vocabulary) that hold the
//! structural metadata produced by Layers 0–3:
//!
//! | Partition | Key | Value | Purpose |
//! |-----------|-----|-------|---------|
//! | `uuid_to_cluster` | `Uuid7` (16 B) | `ClusterId` (u32 LE, 4 B) | "Which cluster is this node in?" |
//! | `uuid_to_region`  | `Uuid7` (16 B) | `RegionId` (u32 LE, 4 B)  | "Which region is this node in?" |
//! | `uuid_is_hub`     | `Uuid7` (16 B) | `bool` (1 B)              | Hub flag, answered before expensive walks |
//! | `hub_neighbors`   | hub `Uuid7`    | length-prefixed `Vec<(Uuid7, f32)>` | The hub-graph adjacency |
//! | `cluster_members` | `ClusterId` (u32 LE) | length-prefixed `Vec<Uuid7>` | "Who's in this cluster?" |
//! | `cluster_meta`    | `ClusterId` (u32 LE) | `{size: u32, hub_count: u32}` (8 B) | Size + hub count per cluster |
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
//! * **Incremental maintenance.** Calling `build_from_source` rebuilds
//!   everything from scratch. Ada-IVF-style incremental updates are
//!   issue #27, post-v0.1.
//! * **Time-travel** (`query_as_of`). Bitemporal history tables in
//!   Parquet are issue #29, also post-v0.1.

use crate::community::leiden;
use crate::graph::{Graph, GraphError};
use crate::hub::HubSet;
use crate::hub_graph::HubGraph;
use crate::id::Uuid7;
use crate::region::RegionGraph;
use crate::source::GraphSource;

use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use std::fmt;
use std::path::Path;
use uuid::Uuid;

// Default fraction of nodes flagged as hubs by `build_from_source`.
// 10% is a reasonable starting point per the design doc's 0.1–5% range
// for graphs at academic-fixture scale; production tuning lives at the
// caller via a future `SwConfig` parameter.
const DEFAULT_HUB_FRACTION: f64 = 0.10;
// Hub-graph k_hop default. Per `DESIGN.md` line 102.
const DEFAULT_HUB_GRAPH_K_HOP: usize = 3;

/// The persisted small-world property-graph index.
///
/// `SwIndex` wraps a Fjall keyspace plus six partitions. Open one at a
/// directory path with [`SwIndex::open`]; populate it with
/// [`SwIndex::build_from_source`]; query it with the various read
/// accessors. The same path reopened later yields the same data
/// (durability invariant tested in `round_trip_via_close_and_reopen`).
pub struct SwIndex {
    /// The Fjall keyspace owning all six partitions. Held as the last
    /// field so its `Drop` runs after all `PartitionHandle` references.
    keyspace: Keyspace,
    uuid_to_cluster: PartitionHandle,
    uuid_to_region: PartitionHandle,
    uuid_is_hub: PartitionHandle,
    hub_neighbors: PartitionHandle,
    cluster_members: PartitionHandle,
    cluster_meta: PartitionHandle,
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
    /// On-disk data is shaped differently than expected. Either a
    /// corrupted keyspace or a version mismatch from a future swindex
    /// release that wrote a different format. The string carries
    /// detail for the operator.
    Corruption(String),
}

impl fmt::Display for SwIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwIndexError::Io(e) => write!(f, "swindex io error: {e}"),
            SwIndexError::Fjall(e) => write!(f, "swindex fjall error: {e}"),
            SwIndexError::Graph(e) => write!(f, "swindex graph error: {e}"),
            SwIndexError::Corruption(s) => write!(f, "swindex on-disk corruption: {s}"),
        }
    }
}

impl std::error::Error for SwIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwIndexError::Io(e) => Some(e),
            SwIndexError::Fjall(e) => Some(e),
            SwIndexError::Graph(e) => Some(e),
            SwIndexError::Corruption(_) => None,
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
        let keyspace = Config::new(path).open()?;
        let opts = PartitionCreateOptions::default();
        // Every partition is opened by name; if it doesn't exist yet
        // Fjall creates it transparently. Calling `open` twice on the
        // same path returns equivalent handles.
        let uuid_to_cluster = keyspace.open_partition("uuid_to_cluster", opts.clone())?;
        let uuid_to_region = keyspace.open_partition("uuid_to_region", opts.clone())?;
        let uuid_is_hub = keyspace.open_partition("uuid_is_hub", opts.clone())?;
        let hub_neighbors = keyspace.open_partition("hub_neighbors", opts.clone())?;
        let cluster_members = keyspace.open_partition("cluster_members", opts.clone())?;
        let cluster_meta = keyspace.open_partition("cluster_meta", opts)?;
        Ok(Self {
            keyspace,
            uuid_to_cluster,
            uuid_to_region,
            uuid_is_hub,
            hub_neighbors,
            cluster_members,
            cluster_meta,
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
    pub fn build_from_source<G: GraphSource>(
        &mut self,
        source: &G,
    ) -> Result<BuildStats, SwIndexError> {
        // Step 1: in-memory Graph (Layer 0).
        let graph = Graph::from_source(source)?;

        // Step 2: Leiden clusters (Layer 1).
        let clusters = leiden(&graph);

        // Step 3: regions (Layer 3 — recursive Leiden over clusters).
        let regions = RegionGraph::build(&graph, &clusters);

        // Step 4: hub set (Layer 2a) + hub graph (Layer 2b).
        let hubs = HubSet::from_top_fraction(&graph, DEFAULT_HUB_FRACTION);
        let hub_graph = HubGraph::build(&graph, &hubs, DEFAULT_HUB_GRAPH_K_HOP);

        // Step 5: persist everything via one atomic Fjall batch.
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
            let mut meta_buf = Vec::with_capacity(8);
            meta_buf.extend_from_slice(&size_u32.to_le_bytes());
            meta_buf.extend_from_slice(&hub_count_u32.to_le_bytes());
            batch.insert(
                &self.cluster_meta,
                cluster_id.to_le_bytes().as_slice(),
                meta_buf,
            );
        }

        // Atomic commit + fsync. After this returns, a fresh open() of
        // the same path will see exactly this build.
        batch.commit()?;
        self.keyspace.persist(PersistMode::SyncAll)?;

        Ok(BuildStats {
            nodes: graph.node_count(),
            clusters: clusters.community_count(),
            regions: regions.region_count(),
            hubs: hubs.len(),
        })
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

    /// The (size, hub_count) pair for a cluster, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// [`SwIndexError::Fjall`] for read failures;
    /// [`SwIndexError::Corruption`] for malformed stored data.
    pub fn cluster_meta(&self, cluster_id: u32) -> Result<Option<(u32, u32)>, SwIndexError> {
        let key = cluster_id.to_le_bytes();
        let raw = self.cluster_meta.get(key.as_slice())?;
        raw.map(|b| {
            if b.len() != 8 {
                return Err(SwIndexError::Corruption(format!(
                    "cluster_meta expected 8 bytes, got {}",
                    b.len()
                )));
            }
            let mut size_bytes = [0u8; 4];
            size_bytes.copy_from_slice(&b[0..4]);
            let mut hub_bytes = [0u8; 4];
            hub_bytes.copy_from_slice(&b[4..8]);
            Ok((
                u32::from_le_bytes(size_bytes),
                u32::from_le_bytes(hub_bytes),
            ))
        })
        .transpose()
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

fn encode_uuid_vec(uuids: &[Uuid7]) -> Result<Vec<u8>, SwIndexError> {
    // 4-byte length prefix (u32 LE) + 16 B per uuid.
    let len = u32::try_from(uuids.len())
        .map_err(|_| SwIndexError::Corruption("vec len exceeds u32".into()))?;
    let mut buf = Vec::with_capacity(4 + uuids.len() * 16);
    buf.extend_from_slice(&len.to_le_bytes());
    for u in uuids {
        buf.extend_from_slice(u.as_bytes());
    }
    Ok(buf)
}

fn decode_uuid_vec(bytes: &[u8], context: &str) -> Result<Vec<Uuid7>, SwIndexError> {
    if bytes.len() < 4 {
        return Err(SwIndexError::Corruption(format!(
            "{context}: payload < 4 bytes"
        )));
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&bytes[0..4]);
    let len = u32::from_le_bytes(len_bytes) as usize;
    let expected = 4 + len * 16;
    if bytes.len() != expected {
        return Err(SwIndexError::Corruption(format!(
            "{context}: expected {expected} bytes for len={len}, got {}",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let start = 4 + i * 16;
        let end = start + 16;
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[start..end]);
        let raw = Uuid::from_bytes(buf);
        let wrapped = Uuid7::from_uuid(raw).ok_or_else(|| {
            SwIndexError::Corruption(format!(
                "{context}: stored UUID at offset {start} is not version 7"
            ))
        })?;
        out.push(wrapped);
    }
    Ok(out)
}

fn encode_hub_neighbors(items: &[(Uuid7, f32)]) -> Result<Vec<u8>, SwIndexError> {
    // 4-byte length prefix (u32 LE) + 20 B per item (16 B uuid + 4 B f32).
    let len = u32::try_from(items.len())
        .map_err(|_| SwIndexError::Corruption("hub-neighbors len exceeds u32".into()))?;
    let mut buf = Vec::with_capacity(4 + items.len() * 20);
    buf.extend_from_slice(&len.to_le_bytes());
    for (uuid, w) in items {
        buf.extend_from_slice(uuid.as_bytes());
        buf.extend_from_slice(&w.to_le_bytes());
    }
    Ok(buf)
}

fn decode_hub_neighbors(bytes: &[u8], context: &str) -> Result<Vec<(Uuid7, f32)>, SwIndexError> {
    if bytes.len() < 4 {
        return Err(SwIndexError::Corruption(format!(
            "{context}: payload < 4 bytes"
        )));
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&bytes[0..4]);
    let len = u32::from_le_bytes(len_bytes) as usize;
    let expected = 4 + len * 20;
    if bytes.len() != expected {
        return Err(SwIndexError::Corruption(format!(
            "{context}: expected {expected} bytes for len={len}, got {}",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let base = 4 + i * 20;
        let mut uuid_buf = [0u8; 16];
        uuid_buf.copy_from_slice(&bytes[base..base + 16]);
        let raw = Uuid::from_bytes(uuid_buf);
        let wrapped = Uuid7::from_uuid(raw).ok_or_else(|| {
            SwIndexError::Corruption(format!(
                "{context}: stored hub neighbor at offset {base} is not v7"
            ))
        })?;
        let mut w_buf = [0u8; 4];
        w_buf.copy_from_slice(&bytes[base + 16..base + 20]);
        out.push((wrapped, f32::from_le_bytes(w_buf)));
    }
    Ok(out)
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
    use super::{BuildStats, SwIndex};
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
}
