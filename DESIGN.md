# Small-World Property-Graph Index — Design Document

A novel database index that exploits emergent small-world topology to make property-graph traversal scale to billions of nodes with O(log N) typical query latency. Standalone database technology — independent of any particular application vertical.

---

## Implementation status

**This is the north-star design document, not a description of the shipped code.** It was written before implementation and describes the target system. For shipped behavior, the rustdoc (`src/lib.rs` and module docs) and [`BENCHMARKS.md`](BENCHMARKS.md) are authoritative. The table below maps each design element to its current state; when code and design diverge in a way this table doesn't capture, update the table — don't let silent drift accumulate (see [`CONTRIBUTING.md`](CONTRIBUTING.md) §8).

| Design element | Designed | Shipped today (v0.1.x main) |
|---|---|---|
| L1 Leiden clustering | Traag 2019, parallel, resolution-tuned | ✅ Implemented (single-threaded, fixed γ=1) |
| L2 hub detection | Degree + betweenness + type, 0.1–1% of nodes | Degree + betweenness implemented; build defaults to the degree ∪ betweenness composite via `SwConfig` (10% per criterion — fixture-scale interim; no type-based criterion yet) |
| L2 hub graph | k-hop BFS adjacency, greedy multi-hop navigation | Adjacency built + persisted; **queries do a one-hop expansion from one entry hub** — no greedy walk yet |
| L3 region graph | Inter-region adjacency, query routing | Cluster→region partition built + persisted; **no region adjacency, not consulted at query time** |
| Query API | `query(pattern, opts)`, `query_as_of`, unanchored entry | `QueryKind::{SameCluster, Similar}` anchored at an existing node; no patterns, no time-travel |
| Build | Parallel, O(N log N), billions of nodes | In-memory, single-threaded, rebuild-only; benchmarked to N=50k |
| Incremental maintenance | Ada-IVF adaptive re-clustering | Phase-1 scaffolding: `insert_node` majority-vote + drift tracking; only policy is `NeverRebalance` |
| Storage | Fjall v3 hot + Parquet/Iceberg cold tier | Fjall v2, nine partitions; **no cold tier** |
| Crate stack | tokio, petgraph, arrow, iceberg, dashmap, … | fjall, serde, tracing, uuid, clap — nothing async, no columnar deps |
| Module layout | Multi-crate workspace, nested modules | Single crate, flat `src/*.rs` |
| Measured query scaling | O(log N) typical | **O(cluster_size)** — see BENCHMARKS.md "What the data does and doesn't show" |

---

## Context

**The problem this solves.** Today's graph databases (Neo4j, IndraDB, Kuzu, Grafeo, ArangoDB, TigerGraph, SurrealDB) store nodes and edges with index-free adjacency and rely on edge-by-edge traversal for queries. They are topology-blind: they treat all edges as equal and do not exploit the structural properties (clustering, hubs, long-range shortcuts) that real-world graphs naturally exhibit. As graphs grow into the hundreds of millions or billions of nodes, query latency degrades because traversal cost grows with edge count, not with the topology of the data.

**What exists today for similar problems.** For vector data, HNSW (Hierarchical Navigable Small World) solves an analogous problem brilliantly: it builds a small-world graph index over vectors and delivers sub-millisecond approximate-nearest-neighbor queries on billion-vector corpora. HNSW is in production in Pinecone, Milvus, Vespa, Lance, Oracle 26, pgvector, Qdrant. **But HNSW only works for vector spaces (geometric similarity).** For arbitrary structured property data — nodes with typed content, edges with semantic meaning, hub structures emerging from authority and reference patterns — no equivalent index exists in production.

**What this index is.** A persistent, online, query-routing index for property graphs that builds and maintains a hierarchical small-world structure over arbitrary graphs of typed nodes connected by typed references. Constructed via Leiden community detection + hub identification + recursive aggregation. Maintained incrementally as facts are added. Queried via hub-aware traversal that achieves O(log N) typical complexity, the same scaling HNSW achieves for vectors.

**The novelty.** Every component is research-mature. Leiden community detection is from 2019 (Traag et al.) with mathematically guaranteed well-connected clusters. Hub detection has decades of academic work behind it. Incremental cluster maintenance was formalized in 2024-2025 by Ada-IVF and related papers. The **integration of these into a persistent, online, query-routing graph index that any application can use** is what does not exist in production.

---

## The foundational research insight

Until late 2024 the conventional reading of HNSW was: "it works because of the hierarchy" — strict layers with randomized assignment exposing long-range shortcuts at the top. A December 2024 paper, **["Down with the Hierarchy: The 'H' in HNSW Stands for 'Hubs'" (arxiv:2412.01940)](https://arxiv.org/pdf/2412.01940)**, makes the case that HNSW's effectiveness actually comes from the emergence of a **hub-rich highway** of long-range bridges among nodes that, regardless of which layer they nominally inhabit, are the structurally important navigation anchors. The hierarchy is one way of exposing those hubs to traversal; what actually matters is that the hubs exist and that queries route through them.

This dramatically simplifies the design for property-graph applications. We do not need to slavishly copy HNSW's strict randomized layer-assignment scheme. We need:

1. **Identify hubs** by structural properties (degree, centrality, type, institutional role).
2. **Build a navigable hub graph** for long-range traversal.
3. **Maintain local cluster structure** for last-mile precision.

Everything else is engineering taste. This frees us to design simpler and more adaptive than vanilla HNSW.

---

## Four-layer architecture

```
┌───────────────────────────────────────────────────────────────────────┐
│  LAYER 3 — REGION GRAPH                                               │
│  • Top-level Leiden communities of clusters                           │
│  • ~thousands of nodes for billions-of-facts substrate                │
│  • Edges weighted by inter-region traffic patterns (workload-derived) │
│  • Purpose: query routing. Which region(s) is this query about?       │
└─────────────────────────────────┬─────────────────────────────────────┘
                                  │
┌─────────────────────────────────┴─────────────────────────────────────┐
│  LAYER 2 — HUB GRAPH ("the highway")                                  │
│  • ~0.1–1 % of all nodes                                              │
│  • Selected by: degree ≥ threshold; betweenness centrality;           │
│    type-nodes (most-instantiated patterns); institutional anchors     │
│    (authorities, registries); structurally pivotal entities           │
│  • Edges: direct + computed shortcut bridges                          │
│  • Purpose: long-range navigation across the substrate                │
└─────────────────────────────────┬─────────────────────────────────────┘
                                  │
┌─────────────────────────────────┴─────────────────────────────────────┐
│  LAYER 1 — CLUSTER GRAPH                                              │
│  • Leiden-detected communities (well-connected by Leiden guarantee)   │
│  • Cluster size 100–10,000 nodes (Leiden resolution-tuned)            │
│  • Inter-cluster edges weighted by query frequency (workload-aware)   │
│  • Purpose: bound the search space after hub navigation lands here    │
└─────────────────────────────────┬─────────────────────────────────────┘
                                  │
┌─────────────────────────────────┴─────────────────────────────────────┐
│  LAYER 0 — FULL FACT GRAPH                                            │
│  • Every node, every edge, ground truth                               │
│  • Stored on disk (Parquet / Iceberg) with hot working set in Fjall   │
│  • Layer 1 holds direct pointers into Layer 0                         │
│  • Purpose: precision — final filtering, exact answers                │
└───────────────────────────────────────────────────────────────────────┘
```

### Differences from vanilla HNSW

- **No randomized layer assignment.** Node placement is determined by structural properties (degree, centrality, cluster membership), not by a random coin flip during insertion.
- **Cluster layer uses Leiden**, which provides mathematically guaranteed well-connected communities, rather than k-nearest-neighbors over a metric space.
- **Workload-aware edge weights.** Inter-region and inter-cluster edges are weighted by actual query frequency. Periodically re-weighted.
- **Heterogeneous content.** Nodes carry typed, structured content. Index respects type when navigating — a query about "HVAC components" preferentially routes through HVAC type-nodes.
- **Persistent + online.** The index is not rebuilt per query — it lives on disk, is maintained incrementally, and serves online queries.

---

## Construction algorithm

Initial bulk-build over an existing graph. Parallelizable; O(N log N) with parallel Leiden implementations.

```text
ALGORITHM: build_index(fact_graph)

1. Detect hubs
   hubs := ∅
   for node in fact_graph:
       if degree(node) ≥ DEGREE_THRESHOLD
          or approx_betweenness(node) ≥ CENTRALITY_THRESHOLD
          or node.type ∈ HUB_TYPES:        // type-nodes, registries, institutions
           hubs.insert(node)
   // Expected: 0.1 – 1 % of nodes are hubs.

2. Construct hub graph
   hub_graph := NavigableGraph::new()
   for h1 in hubs:
       neighbors := find_k_hop_neighbors(h1, k = 3) ∩ hubs
       for h2 in neighbors:
           hub_graph.add_edge(h1, h2, weight = path_quality(h1, h2))

3. Detect clusters (Leiden, parallel)
   clusters_l1 := leiden(fact_graph, resolution = COARSE)
   // Recursive subdivide for fine-grained communities within
   l1_to_l2 := leiden_recursive(clusters_l1, resolution = FINE)

4. Detect regions (top-level Leiden over cluster graph)
   cluster_graph := condense_clusters_to_supergraph(clusters_l1, fact_graph)
   regions := leiden(cluster_graph, resolution = VERY_COARSE)

5. Build cross-reference indexes
   for node in fact_graph:
       persist_mapping(node.uuid → cluster_id)
       persist_mapping(node.uuid → region_id)
   for cluster in clusters_l1:
       persist_mapping(cluster.id → [member_hubs])
       persist_mapping(cluster.id → region_id)

6. Persist
   Hot (Fjall):    uuid → cluster_id, cluster → region_id, hub_neighbors,
                   cluster metadata (size, modularity, query frequency)
   Cold (Parquet): cluster_assignment_history (for time-travel),
                   region structure history
```

**Complexity.** Leiden has parallel implementations running at O(N log N) for billions of edges (see [Fast Leiden Algorithm for Community Detection in Shared Memory Setting, ACM 2024](https://dl.acm.org/doi/fullHtml/10.1145/3673038.3673146)). Hub detection by degree is O(E); approximate betweenness centrality can run in O(N + E) with sampling. The total bulk-build is feasible for billions of nodes given parallel Leiden + approximate centrality.

---

## Query algorithm

Routes a graph traversal query through the four layers. Achieves O(log N) typical complexity.

```text
ALGORITHM: query(pattern, start_seed = None)

1. Resolve entry point
   if start_seed:
       entry := start_seed
   else:
       candidate_hubs := match_pattern_against_hubs(pattern)
       entry := best_candidate(candidate_hubs)

2. Region-level routing
   current_region := entry.region
   if pattern_implies_other_regions(pattern):
       relevant_regions := find_relevant_regions(pattern)
   else:
       relevant_regions := [current_region]

3. Hub-graph navigation
   candidate_hubs := ∅
   for region in relevant_regions:
       // Greedy walk through hub_graph toward pattern-matching destinations.
       // Like HNSW upper-layer traversal but in hub space.
       candidate_hubs ∪= navigate_hubs(region, pattern)

4. Cluster expansion
   candidate_clusters := ∅
   for hub in candidate_hubs:
       candidate_clusters ∪= hub.clusters    // each hub belongs to 1+ clusters

5. Within-cluster precision traversal
   // Now we have a small bounded set (typically 5–50 clusters of 100–10K nodes).
   // Walk Layer 0 within these clusters only.
   results := ∅
   for cluster in candidate_clusters:
       results ∪= traverse_within_cluster(cluster, pattern)

6. Return ranked results
   return rank_and_return(results, pattern.ranking_criteria)
```

**Complexity.**
- Hub navigation: O(log N) — small-world property of the hub graph.
- Cluster expansion: O(k) where k is small (5–50 clusters per typical query).
- Within-cluster precision: O(s) where s is bounded by Leiden's cluster size (configurable, typically ≤ 10,000).
- **Total: O(log N + k · s)** — effectively O(log N) for queries that match the index's clustering structure.

For queries that do not match the cluster structure (e.g., a query that spans every region uniformly), the index degrades gracefully to scanning Layer 0 with the help of bloom filters and column statistics on Iceberg. Worst case is bounded by the standard columnar-scan complexity, not infinite.

---

## Incremental maintenance algorithm

The graph evolves continuously. Re-indexing from scratch is wasteful. Adopts the **adaptive maintenance pattern from Ada-IVF** ([arxiv:2411.00970, Nov 2024](https://arxiv.org/html/2411.00970v1)).

```text
ALGORITHM: on_new_fact(node, edges)

1. Cluster assignment
   neighbor_clusters := { e.target.cluster_id for e in edges }
   node.cluster_id := majority_vote(neighbor_clusters)
       // tie-break by spatial / type / temporal proximity
   persist_assignment(node, node.cluster_id)

2. Hub candidacy check
   if predicted_degree(node) ≥ HUB_THRESHOLD
      or node.type ∈ HUB_TYPES:
       promote_to_hub(node)
       hub_graph.add_node(node, edges = hub_neighbors_of(node))

3. Cluster health tracking
   c := clusters[node.cluster_id]
   c.delta_count += 1
   c.delta_quality_score += modularity_change(node, edges)
   c.query_frequency_decay()

4. Re-Leiden trigger evaluation
   if c.delta_count > REINDEX_THRESHOLD
      or c.delta_quality_score < QUALITY_THRESHOLD
      or c.write_rate > c.read_rate * THRESHOLD_RATIO:
       enqueue_local_releiden(c)    // async background

5. Periodic global maintenance (background)
   every N hours:
       sample_workload()
       identify_hot_cold_regions()
       reshape_region_graph_edges()
       re_weight_inter_cluster_edges()
       optionally promote/demote hubs based on observed traversal
```

### Cluster split / merge

When local re-Leiden produces a different partition than the existing one:

- **Split.** Original cluster C produces { C1, C2 }. New cluster nodes get UUID7s. Original C marked superseded; predecessor edges link from C → { C1, C2 }. All Layer-0 node-to-cluster mappings updated atomically.
- **Merge.** Two adjacent clusters C1, C2 produce a single C. C minted with new UUID7; predecessor edges link from { C1, C2 } → C.
- **History preserved.** The cluster_assignment_history table retains the prior assignment with `valid_to = now()`; the new assignment gets `valid_from = now(), valid_to = null`. **Time-travel queries can resolve cluster structure at any past moment.**

### Throughput characteristics

Per the Ada-IVF research, adaptive local re-clustering delivers approximately 5× higher update throughput than naive whole-index rebuild approaches. For property graphs with sustained 10K-100K writes/sec, this translates to manageable background re-Leiden activity affecting only the few clusters that have actually drifted.

---

## Concrete data structures

### Hot (Fjall — embedded LSM, sub-millisecond access)

```text
keyspace "swindex.hot" {

  // Forward indexes
  uuid_to_cluster:        UUID7      → ClusterID
  uuid_to_region:         UUID7      → RegionID
  uuid_is_hub:            UUID7      → bool

  // Hub graph
  hub_neighbors:          UUID7      → Vec<(UUID7, weight: f32)>
  hub_clusters:           UUID7      → Vec<ClusterID>

  // Cluster metadata
  cluster_size:           ClusterID  → u64
  cluster_hubs:           ClusterID  → Vec<UUID7>
  cluster_modularity:     ClusterID  → f64
  cluster_delta_count:    ClusterID  → u64
  cluster_query_freq:     ClusterID  → f64
  cluster_predecessor:    ClusterID  → Vec<ClusterID>   // for split/merge history

  // Region metadata
  region_clusters:        RegionID   → Vec<ClusterID>
  region_neighbors:       RegionID   → Vec<(RegionID, weight: f32)>
}
```

### Cold (Parquet / Iceberg — columnar, scan-friendly, time-travel)

```text
table "swindex.cluster_assignment_history" {
  // Append-only history; supports time-travel queries.
  uuid:           UUID7
  cluster_id:     ClusterID
  valid_from:     Timestamp
  valid_to:       Timestamp NULL    // null if currently active
  asserted_by:    Signature
}

table "swindex.hub_history" {
  // When a node was promoted/demoted as hub.
  uuid:           UUID7
  is_hub:         bool
  valid_from:     Timestamp
  valid_to:       Timestamp NULL
}

table "swindex.region_structure_history" {
  // Snapshots of region graph; used for time-travel + analysis.
  snapshot_id:    UUID7
  taken_at:       Timestamp
  payload:        Bytes              // serialized region graph
}
```

The split between hot and cold tracks **what's needed at query time vs what's needed for offline analysis / time-travel**. Hot must be fast; cold must be exhaustive.

---

## Why this design beats existing approaches

| Approach | What it does | Why it's worse |
|---|---|---|
| Vanilla graph DB (Neo4j, IndraDB, Kuzu, Grafeo) | Index-free adjacency, edge-by-edge walks | No topology awareness; degrades at scale; no clustering or hub structure |
| Vanilla HNSW | Hierarchical small-world for vectors | Only works for embedding space, not arbitrary structured properties |
| Pure Leiden + table lookup | Detect clusters, scan within | No hub-level navigation; no long-range shortcuts; no online maintenance |
| Random sampling for index | Pick N nodes as bridges | Hubs are not random — they are structurally important; random sampling misses them |
| Full materialized views | Pre-compute every query | Doesn't scale; cannot stay current; storage explosion |
| Microsoft GraphRAG | Recursive Leiden for analysis | One-shot offline summarization, not an online query-routing index |
| Adjacency lists per node (some graph DBs) | Memory-efficient adjacency | No topology summary; every query starts from scratch |

This design combines: **the structural rigor of Leiden** (well-connected clusters with mathematical guarantee), **the navigability of HNSW** (logarithmic traversal through emergent hubs), **the hub insight from the Dec 2024 paper** (which reframes what makes HNSW actually work), and **the adaptive maintenance from streaming-index research** (Ada-IVF) — applied to property graphs, persisted to disk, online-maintained, query-routing.

It is a synthesis of multiple research threads that none of them addresses alone.

---

## Rust implementation

### Crate stack (only the index, not a full database)

| Concern | Crate(s) |
|---|---|
| Async runtime | `tokio` |
| Graph primitives | `petgraph`, `graphalgs` |
| Embedded LSM (hot) | `fjall` (v2 shipped today) |
| Columnar (cold) | `arrow-rs`, `parquet`, `iceberg-rust` |
| Object storage | `object_store` |
| Identity | `uuid` crate (v7) |
| Concurrency primitives | `dashmap`, `parking_lot`, `crossbeam` |
| Tracing | `tracing` |
| Property index for vectors (coexists) | `hnsw_rs` or `swarc` |
| Full-text (coexists) | `tantivy` |

Leiden community detection and approximate centrality are implemented from scratch in Rust (~2,000 LoC). No production Rust Leiden crate exists yet; this is engineering work but not novel research.

### Module layout

```
swindex/
├── Cargo.toml
├── src/
│   ├── lib.rs              // Public API: insert_node, query, maintain
│   ├── identity.rs         // UUID7 type, sortable + temporal
│   ├── node.rs             // Node + edge structs; serialization
│   ├── graph.rs            // In-memory graph wrapper over petgraph
│   ├── cluster/
│   │   ├── leiden.rs       // Leiden community detection (parallel)
│   │   ├── modularity.rs   // Modularity calculation + change-tracking
│   │   └── recursive.rs    // Hierarchical Leiden (region detection)
│   ├── hub/
│   │   ├── detect.rs       // Hub candidacy: degree, centrality, type
│   │   ├── betweenness.rs  // Approximate betweenness centrality
│   │   └── graph.rs        // Hub graph construction + persistence
│   ├── layers/
│   │   ├── region.rs       // Layer 3: region graph
│   │   ├── hubs.rs         // Layer 2: hub graph navigation
│   │   ├── clusters.rs     // Layer 1: cluster graph
│   │   └── full.rs         // Layer 0: full fact graph access
│   ├── query/
│   │   ├── planner.rs      // Routes queries through layers
│   │   ├── traversal.rs    // Hub-aware navigation
│   │   └── pattern.rs      // Graph pattern matching
│   ├── maintenance/
│   │   ├── insert.rs       // on_new_fact: cluster assignment, hub check
│   │   ├── triggers.rs     // Re-Leiden trigger evaluation
│   │   ├── local_releiden.rs // Local re-cluster (Ada-IVF style)
│   │   └── periodic.rs     // Background workload-aware reshaping
│   ├── storage/
│   │   ├── hot.rs          // Fjall-backed hot index
│   │   └── cold.rs         // Iceberg-backed history + analytics
│   └── tests/
│       ├── construction.rs
│       ├── query.rs
│       ├── maintenance.rs
│       ├── time_travel.rs
│       └── workload.rs
```

### Estimated size

| Module | Lines |
|---|---|
| Leiden + modularity + recursive | ~2,000 |
| Hub detection + betweenness + hub graph | ~1,500 |
| Layer construction + persistence | ~1,500 |
| Query planner + traversal | ~2,000 |
| Maintenance (insert, triggers, local re-Leiden, periodic) | ~2,000 |
| Storage (hot Fjall + cold Iceberg) | ~1,500 |
| Public API + integration glue | ~1,500 |
| Tests | ~3,000 |
| **Total** | **~15,000 lines of Rust** |

### Public API surface

```rust
pub struct SwIndex { /* private */ }

impl SwIndex {
    pub fn open(path: &Path, config: SwConfig) -> Result<Self>;

    // Bulk build from existing graph
    pub fn build_from_graph<G: GraphSource>(&mut self, source: G) -> Result<BuildStats>;

    // Insertion (single fact)
    pub fn insert_node(&mut self, node: Node) -> Result<()>;
    pub fn insert_edge(&mut self, from: Uuid7, to: Uuid7, edge_type: EdgeType) -> Result<()>;

    // Query
    pub fn query(&self, pattern: Pattern, opts: QueryOpts) -> Result<QueryResult>;
    pub fn query_as_of(&self, pattern: Pattern, ts: Timestamp, opts: QueryOpts) -> Result<QueryResult>;

    // Maintenance (usually background)
    pub fn maintain(&mut self) -> Result<MaintenanceReport>;
    pub fn force_releiden(&mut self, cluster: ClusterId) -> Result<()>;

    // Introspection
    pub fn stats(&self) -> SwStats;
    pub fn cluster_of(&self, uuid: Uuid7) -> Option<ClusterId>;
    pub fn is_hub(&self, uuid: Uuid7) -> bool;
}
```

This is a self-contained library. An application embeds it like SQLite: open a path, insert facts, query patterns. No assumed application semantics. No authentication. No API server. **Just the index.** The application built on top of it (real-estate substrate, knowledge graph, agent memory, etc.) provides those layers separately.

---

## Testing strategy

### Correctness

1. **Leiden correctness.** Use known reference graphs (Zachary's karate club, LFR benchmark networks) with established ground-truth community structures. Validate clusters match expected partitions to within published modularity tolerances.
2. **Hub detection correctness.** On synthetic scale-free graphs (Barabási-Albert model), validate that the index identifies the high-degree hubs and that the hub graph is genuinely small-world (clustering coefficient + path length within Watts-Strogatz bounds).
3. **Query correctness.** For every traversal query, validate that the indexed result matches the result of an exhaustive Layer-0 traversal. The index should never return wrong results — only fewer, faster results.
4. **Time-travel correctness.** Apply a sequence of cluster split/merge events; query the index at intermediate timestamps; validate the historical structure is recoverable.

### Performance

1. **Construction scaling.** Bulk-build graphs of 10⁴, 10⁵, 10⁶, 10⁷ nodes; measure build time; validate O(N log N).
2. **Query scaling.** For graphs at each scale, measure latency on representative multi-hop pattern queries; validate O(log N) typical.
3. **Update throughput.** Sustained insert rate; measure background re-Leiden overhead; validate ≥ 5× improvement over naive whole-rebuild (per Ada-IVF target).
4. **Memory footprint.** Hot index (Fjall) vs cold index (Iceberg); validate hot index fits in memory for typical working sets.

### Workload realism

1. **Real-world graph datasets.** Use SNAP datasets (Stanford Network Analysis Platform): web graphs, citation networks, road networks, social networks. Validate index behaves on real graphs.
2. **Synthetic small-world generators.** Watts-Strogatz, Barabási-Albert, Kleinberg models. Validate the index detects and exploits the planted structure.
3. **Adversarial workloads.** Anti-clustering patterns (random uniform edges), highly dynamic streams (cluster boundaries shift rapidly), pathological hub distributions. Validate graceful degradation, no correctness regressions.

---

## Research foundation

Required reading in priority order:

1. **["Down with the Hierarchy: The 'H' in HNSW Stands for 'Hubs'" (arxiv:2412.01940, Dec 2024)](https://arxiv.org/pdf/2412.01940)** — reframes HNSW's mechanism as hub-driven; foundational for this design.
2. **["From Louvain to Leiden: guaranteeing well-connected communities" (Traag, Waltman, van Eck, Nature Sci Rep 2019)](https://www.nature.com/articles/s41598-019-41695-z)** — Leiden algorithm.
3. **["Fast Leiden Algorithm for Community Detection in Shared Memory Setting" (ACM 2024)](https://dl.acm.org/doi/fullHtml/10.1145/3673038.3673146)** — parallel Leiden.
4. **["Maintaining Leiden Communities in Large Dynamic Graphs" (arxiv:2601.08554)](https://arxiv.org/pdf/2601.08554)** — incremental cluster maintenance.
5. **["Incremental IVF Index Maintenance for Streaming Vector Search" (Ada-IVF, arxiv:2411.00970)](https://arxiv.org/html/2411.00970v1)** — adaptive maintenance pattern.
6. **["Microsoft GraphRAG — From Local to Global" (arxiv:2404.16130)](https://arxiv.org/html/2404.16130v2)** — recursive Leiden for hierarchical summarization at scale.
7. **["Memory-Efficient Community Detection on Large Graphs Using Weighted Sketches" (arxiv:2411.02268)](https://arxiv.org/html/2411.02268v2)** — sketch-based community detection for very large graphs.
8. **["Optimal Scale-Free Small-World Graphs with Minimum Scaling of Cover Time" (arxiv:2302.06372)](https://arxiv.org/pdf/2302.06372)** — theoretical foundation for scale-free small-world structure.
9. **["Hierarchical Navigable Small World" (Malkov & Yashunin, original HNSW paper)](https://arxiv.org/abs/1603.09320)** — the algorithm this design generalizes from.

---

## Open design questions

These are deferred (not blocking the initial design) but will need empirical decisions during implementation:

1. **Hub-promotion thresholds.** Degree threshold for hub promotion: absolute (e.g., ≥ 100) or relative (e.g., top 0.5% by degree)? Likely workload-dependent.
2. **Leiden resolution parameter tuning.** Cluster size 100–10K is a wide range. Application workload determines the sweet spot — smaller clusters mean more layer-1 nodes but tighter precision per cluster; larger clusters mean cheaper layer-1 but more layer-0 work per query.
3. **When to demote hubs.** A hub whose degree drops below threshold — demote immediately, lazily, or never? Affects index churn vs accuracy.
4. **Region-graph edge weighting.** Workload-derived weights need a decay function (recent queries weighted more) and bootstrap behavior (when no query history exists).
5. **Cluster split-merge frequency.** Conservative split-merge keeps queries stable but accepts some cluster drift; aggressive split-merge keeps clusters fresh but introduces query-result instability across timestamps.
6. **Cold-start hub bootstrap.** When the graph is empty, there are no hubs to navigate. Seed with structural hubs (type-nodes, registry nodes) provided as configuration before any data exists.
7. **Hybrid query planning.** When does a query bypass the index entirely (e.g., it's faster to scan Layer 0 columnar with bloom filters)? Need a cost-based planner.
8. **Persistence atomicity.** Cluster reassignment touches many keys. Use Fjall's snapshot semantics + write-batches for atomic updates. Need explicit transaction boundary.

---

## What this index is not

To be unambiguous about scope:

- **Not a database.** It is an index that an application or database embeds. No transactions, no SQL parser, no access control, no networking API. Those belong to the layer above.
- **Not an application.** It does not assume real estate, knowledge graphs, agent memory, or any other vertical. Any application whose data is shaped as a property graph can use it.
- **Not a vector index.** HNSW and similar exist and are mature for vectors. This index addresses property-graph traversal, which HNSW cannot do. They coexist — an application can use this index for graph queries and HNSW for vector queries against the same node set.
- **Not a graph store.** It indexes a graph that lives elsewhere (typically as facts in Parquet/Iceberg). The index holds metadata + pointers + cluster structure, not the primary node content.
- **Not a query language.** The query API takes pattern specifications programmatically. A query language (Cypher-like, Datalog-like, custom) is a separate layer that translates user-facing queries into the index's pattern API.

---

## Why ship this as a standalone library

The temptation in projects like this is to bundle the index with one specific application (e.g., the real-estate substrate). Resist that temptation. Reasons:

1. **The index is general-purpose.** It solves an unsolved problem for any application with billion-scale property-graph workloads. Many verticals can use it.
2. **Testing benefits from generality.** Validating on synthetic graphs + real-world datasets (SNAP, citation networks, etc.) gives stronger correctness guarantees than testing on one application's data.
3. **Ecosystem benefits from open release.** Publishing the source creates community contributions, bug reports, hardening, and the implicit reputation that comes from being adopted by other projects. (Decided: source-available under BSL 1.1, auto-converting to Apache 2.0 four years after each release — see `LICENSE`.)
4. **Patent/defensibility cleanly maps to the library.** The novelty is in the index design; the application is conventional. Separating them makes the IP picture clearer.
5. **Application-specific layers stay clean.** Real-estate-specific code (county data ingestion, authority signatures, agent APIs) belongs in a higher layer, not entangled with index internals.

The library is named **swindex** ("small-world index") for clarity. License decided: BSL 1.1 (source-available, not open source per the BSL's own text), auto-converting to Apache 2.0 four years after each release.

---

## Next concrete steps

1. **Set up `swindex/` Cargo workspace.** Empty crates: `swindex-core`, `swindex-leiden`, `swindex-hub`, `swindex-storage`. CI scaffolding.
2. **Implement `swindex-leiden`.** Parallel Leiden over petgraph. Validate on Zachary karate, LFR benchmarks.
3. **Implement `swindex-hub`.** Degree-based detection + approximate betweenness centrality. Hub graph data structure.
4. **Implement `swindex-storage`.** Fjall hot index + iceberg-rust cold storage. Persistence layer with snapshots.
5. **Implement `swindex-core::SwIndex`.** Wire everything together. Public API as specified above.
6. **Validation suite.** Correctness tests on reference graphs; performance benchmarks; workload simulation.
7. **Documentation.** README, design doc (this file), API docs, examples.
8. **Public release** (decided: BSL 1.1 source-available). Submit to crates.io.

Estimated effort: **6–10 weeks for one experienced Rust engineer** to ship a usable v0.1 of the standalone library.

---

## Relationship to applications built on top

Applications layer their own concerns on top of `swindex`:

```text
APPLICATION  (e.g., real-estate substrate, agent memory layer, knowledge graph)
  • Vertical data ingestion
  • Authority + signature semantics
  • Capability tokens / access control
  • PR / branch / verification workflow
  • Agent-native API (MCP, etc.)
  • Pricing / billing
  • Federation across instances
        │
        ▼  (uses)
SWINDEX LIBRARY  (this design)
  • Hub detection, Leiden, hub-graph navigation
  • Query routing through 4 layers
  • Incremental maintenance
  • Persistence (hot + cold)
        │
        ▼  (uses)
FOUNDATION CRATES
  • Apache Arrow + Parquet + Iceberg-rust
  • Fjall (v2 shipped today)
  • object_store
  • petgraph
```

The application provides the business model and the vertical specificity. The library provides the index. Clean separation. Each can evolve independently. Each can be open-sourced or commercialized on its own schedule with its own license.
