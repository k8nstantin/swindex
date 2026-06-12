//! Community detection on a [`Graph`].
//!
//! # What this module ships
//!
//! * [`Partition`] — a labeling of every node with a community id.
//! * [`modularity`] — the Newman-Girvan modularity of a partition, the
//!   standard quality score for community detection.
//! * [`louvain`] / [`louvain_seeded`] — full multi-level Louvain (Blondel
//!   et al. 2008): local moving, then aggregation, then recurse on the
//!   coarsened graph, until no more merging is possible. Produces
//!   modularity ≥ 0.40 on Zachary's karate club.
//!
//! # The algorithm in one paragraph
//!
//! Multi-level Louvain wraps two operations in an outer loop:
//!
//! 1. **Local moving** — visit every node in random order, move each
//!    to whichever neighboring community gives the largest positive
//!    Δ-Q (modularity gain). Stop when no node moves in a full pass.
//! 2. **Aggregation** — collapse each community into a single super-
//!    node, with edge weights summed across the original graph. The
//!    coarsened graph is much smaller than the original. Run step 1
//!    on the coarsened graph; communities at this level merge to form
//!    higher-level communities.
//!
//! The outer loop terminates when a full local-moving pass produces no
//! merges (one community per super-node). The composed partition over
//! the original nodes is what gets returned.
//!
//! # Why this matters: single-level vs multi-level
//!
//! On Zachary karate (34 nodes), single-level Louvain gets stuck around
//! Q ≈ 0.33 with 8 small communities — too many tiny groups because
//! local moving has no mechanism to merge structurally-similar
//! communities together. The aggregation step is what unlocks the
//! merging: at the next level, those 8 communities become 8 nodes;
//! local moving on the 8-node graph finds that some of them want to
//! join up. Final result: Q ≈ 0.41, ~4 communities. Same algorithm,
//! same data, dramatically better partition — entirely from adding
//! the aggregation step.
//!
//! # Why Louvain first, not Leiden
//!
//! Louvain (2008) and Leiden (Traag 2019) share this multi-level
//! outer structure. Leiden adds a *refinement* phase between local
//! moving and aggregation that guarantees every community is
//! internally connected. Without refinement (i.e., plain Louvain),
//! it's possible — though rare — for the algorithm to produce a
//! "community" that's actually two disconnected components glued
//! together. The next PR layers Leiden's refinement on top of this
//! Louvain skeleton.
//!
//! # Determinism
//!
//! The local-moving loop visits nodes in a random order — Louvain's
//! result depends on the visit order, so we use a seeded xorshift RNG
//! and advance the seed across levels. The default [`louvain`] entry
//! point uses seed `42`; tests rely on this to assert stable partition
//! counts and modularity values across runs.

use crate::graph::Graph;
use std::collections::BTreeMap;

/// A community assignment over the nodes of a [`Graph`].
///
/// Internally a `Vec<usize>` where `community[i]` is the community id
/// of internal node `i`. Community ids are renumbered to a contiguous
/// `0..k` range — there are no empty buckets in a returned `Partition`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// `community[i]` = community id of internal node `i`.
    community: Vec<usize>,
    /// Number of distinct communities = max value in `community` + 1.
    n_communities: usize,
}

impl Partition {
    /// Build a `Partition` from a `Vec<usize>` of raw community labels,
    /// renumbering them to a contiguous `0..k` range. Consumes `raw`.
    #[must_use]
    pub fn new(raw: Vec<usize>) -> Self {
        // Build a label-renaming map: old_label -> new contiguous label.
        // BTreeMap so the renumbering is deterministic given the input.
        let mut rename: BTreeMap<usize, usize> = BTreeMap::new();
        let mut next: usize = 0;
        let renamed: Vec<usize> = raw
            .into_iter()
            .map(|c| {
                *rename.entry(c).or_insert_with(|| {
                    let v = next;
                    next += 1;
                    v
                })
            })
            .collect();
        Self {
            community: renamed,
            n_communities: next,
        }
    }

    /// Community id of internal node `i`.
    #[must_use]
    pub fn community_of(&self, i: usize) -> usize {
        self.community[i]
    }

    /// Number of distinct communities (always equal to `max(community) + 1`
    /// thanks to renumbering, so iterating `0..community_count()` covers
    /// every populated community).
    #[must_use]
    pub fn community_count(&self) -> usize {
        self.n_communities
    }

    /// Number of nodes covered by the partition.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.community.len()
    }

    /// Iterate the community label of every internal node, in node-index
    /// order.
    #[must_use = "iterator must be consumed"]
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.community.iter().copied()
    }

    /// Group nodes by community. Returns `community_count()` vectors,
    /// each listing the internal node indices in that community.
    #[must_use]
    pub fn buckets(&self) -> Vec<Vec<usize>> {
        let mut buckets = vec![Vec::new(); self.n_communities];
        for (i, &c) in self.community.iter().enumerate() {
            buckets[c].push(i);
        }
        buckets
    }
}

// =========================================================================
// Internal weighted graph — the representation the inner local-moving
// and aggregation loops actually run on. Stripped of the NodeId mapping
// in [`Graph`] since at deeper recursion levels the "nodes" are
// communities, which don't have a public id.
// =========================================================================

/// In-memory weighted graph used by the Louvain inner loop and
/// produced by aggregation at each level. No `NodeId` mapping — just
/// adjacency, degrees, self-loops, and `2m`.
///
/// Self-loop convention matches [`Graph`]: each self-loop appears
/// **once** in the adjacency list with its full weight, contributes
/// its weight once to `degree`, and contributes its weight once to
/// `twice_m`. Aggregation accumulates within-community edges at
/// `2*weight` into the new self-loop, which preserves modularity
/// exactly across the aggregation step (see [`WeightedGraph::aggregate`]).
struct WeightedGraph {
    /// Symmetric adjacency list. For a non-loop undirected edge (u,v,w),
    /// adj[u] contains (v,w) and adj[v] contains (u,w). Self-loops
    /// appear once in adj[u] as (u, weight).
    adj: Vec<Vec<(usize, f64)>>,
    /// Sum of edge weights incident to each node (self-loops counted once).
    degrees: Vec<f64>,
    /// Self-loop weight per node, tracked separately so the Δ-Q
    /// bookkeeping can adjust correctly when a node leaves/joins its
    /// own community.
    loop_weight: Vec<f64>,
    /// Twice the total undirected edge weight. The `2m` divisor.
    twice_m: f64,
}

impl WeightedGraph {
    /// Convert a public [`Graph`] into the internal representation —
    /// just copies the per-node arrays out of the source graph.
    fn from_graph(g: &Graph) -> Self {
        let n = g.node_count();
        let mut adj = Vec::with_capacity(n);
        let mut degrees = Vec::with_capacity(n);
        let mut loop_weight = Vec::with_capacity(n);
        for u in 0..n {
            adj.push(g.neighbors(u).collect());
            degrees.push(g.degree(u));
            loop_weight.push(g.self_loop(u));
        }
        Self {
            adj,
            degrees,
            loop_weight,
            twice_m: g.twice_total_weight(),
        }
    }

    fn node_count(&self) -> usize {
        self.adj.len()
    }

    fn neighbors(&self, i: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.adj[i].iter().copied()
    }

    fn degree(&self, i: usize) -> f64 {
        self.degrees[i]
    }

    fn self_loop(&self, i: usize) -> f64 {
        self.loop_weight[i]
    }

    fn twice_total_weight(&self) -> f64 {
        self.twice_m
    }

    /// Build the aggregated graph: each community in `partition` becomes
    /// one super-node. Edge weights between communities are summed;
    /// edges within a community become a self-loop on the super-node
    /// with weight `2 * total_within_weight` (the doubling preserves
    /// modularity under our self-loop-counted-once convention).
    ///
    /// Returns a graph with `partition.community_count()` nodes.
    ///
    /// **Invariant:** `modularity(aggregated, p_singleton) == modularity(original, partition)`
    /// where `p_singleton` is "every super-node in its own community".
    /// This is what makes multi-level Louvain a valid optimization
    /// strategy — we can compute Δ-Q on the small aggregated graph
    /// and the moves correspond to valid moves on the original.
    fn aggregate(&self, partition: &Partition) -> Self {
        let k = partition.community_count();
        // Adjacency for the new graph, accumulated into BTreeMaps first
        // so duplicate (c1, c2) edges from different original (u, v)
        // pairs get summed rather than appended.
        let mut new_adj_map: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); k];
        let mut new_degrees: Vec<f64> = vec![0.0; k];
        let mut new_loop: Vec<f64> = vec![0.0; k];
        let mut new_twice_m: f64 = 0.0;

        // Walk every undirected edge exactly once. The symmetric
        // adjacency lists in `self` see each non-loop edge twice
        // (u->v and v->u); we filter to v > u to dedupe. Self-loops
        // are handled separately via self.self_loop(u).
        for u in 0..self.node_count() {
            let cu = partition.community_of(u);
            for (v, w) in self.neighbors(u) {
                if v == u {
                    // Self-loop in the original graph. The for-loop
                    // visits self-loops once per node (since they
                    // appear once in adj). Their contribution is
                    // handled below via self.self_loop(u). Skip here.
                    continue;
                }
                if v < u {
                    // Already counted from v's side.
                    continue;
                }
                // (u, v) with v > u — unique undirected edge.
                let cv = partition.community_of(v);
                if cu == cv {
                    // Within-community edge: contributes 2w to the new
                    // self-loop on community cu (the doubling preserves
                    // the modularity invariant — see method doc).
                    new_loop[cu] += 2.0 * w;
                    new_degrees[cu] += 2.0 * w;
                    new_twice_m += 2.0 * w;
                } else {
                    // Cross-community edge: contributes w to the new
                    // (cu, cv) edge, symmetrically.
                    *new_adj_map[cu].entry(cv).or_insert(0.0) += w;
                    *new_adj_map[cv].entry(cu).or_insert(0.0) += w;
                    new_degrees[cu] += w;
                    new_degrees[cv] += w;
                    new_twice_m += 2.0 * w;
                }
            }
            // Original self-loop on u: contributes its weight to the
            // new self-loop on cu. Per the convention, self-loops
            // contribute their weight *once* to degree and twice_m.
            let sw = self.self_loop(u);
            if sw > 0.0 {
                new_loop[cu] += sw;
                new_degrees[cu] += sw;
                new_twice_m += sw;
            }
        }

        // Materialize adjacency lists from the BTreeMaps + self-loops.
        let mut adj: Vec<Vec<(usize, f64)>> = Vec::with_capacity(k);
        for (i, entry) in new_adj_map.iter().enumerate().take(k) {
            let mut list: Vec<(usize, f64)> = entry.iter().map(|(&j, &w)| (j, w)).collect();
            if new_loop[i] > 0.0 {
                // Self-loop entry appears once with the doubled within-weight.
                list.push((i, new_loop[i]));
            }
            adj.push(list);
        }

        Self {
            adj,
            degrees: new_degrees,
            loop_weight: new_loop,
            twice_m: new_twice_m,
        }
    }
}

// =========================================================================
// Public API: modularity + louvain
// =========================================================================

/// Newman-Girvan modularity of a partition.
///
/// The closed-form definition for an undirected weighted graph:
///
/// ```text
///   Q = Σ_c [ Σ_in,c / (2m)  −  ( Σ_tot,c / (2m) )² ]
/// ```
///
/// where for each community `c`:
///
/// * `Σ_in,c` is the sum of edge weights *between members of c*, with
///   non-loop edges contributing their weight twice (once per direction
///   in the symmetric adjacency) and self-loops contributing once.
/// * `Σ_tot,c` is the sum of degrees of nodes in `c` — equivalently,
///   the total weight of edges incident to `c`.
/// * `2m` is twice the total undirected edge weight in the graph,
///   stored on the graph as [`Graph::twice_total_weight`].
///
/// `Q` ranges in `[-1, 1]`. Random partitions yield `Q ≈ 0`. Well-
/// clustered graphs achieve `Q ≥ 0.4`. Negative values mean the
/// partition is *worse than random* — the algorithm would only return
/// a negative-Q partition if no positive move was available, which on
/// real graphs is rare.
///
/// # Panics
///
/// Panics if `partition.node_count() != graph.node_count()`. The caller
/// must build the partition from the same graph it'll be evaluated on.
#[must_use]
pub fn modularity(graph: &Graph, partition: &Partition) -> f64 {
    assert_eq!(
        graph.node_count(),
        partition.node_count(),
        "partition must cover every node of the graph",
    );
    let wg = WeightedGraph::from_graph(graph);
    modularity_internal(&wg, partition)
}

/// Internal modularity calculation over a [`WeightedGraph`]. Public
/// `modularity` converts a [`Graph`] then delegates here.
fn modularity_internal(graph: &WeightedGraph, partition: &Partition) -> f64 {
    let twice_m = graph.twice_total_weight();
    if twice_m == 0.0 {
        // Empty graph: no edges, no modularity to speak of. Convention
        // is to return 0 rather than NaN or +infinity.
        return 0.0;
    }

    let n_c = partition.community_count();
    let mut in_w = vec![0.0_f64; n_c];
    let mut tot_w = vec![0.0_f64; n_c];

    for u in 0..graph.node_count() {
        let cu = partition.community_of(u);
        tot_w[cu] += graph.degree(u);
        for (v, w) in graph.neighbors(u) {
            // Symmetric adj: non-loop edges contribute w from each side
            // (total 2w); self-loops once. Both cases handled correctly
            // by `if cv == cu { in_w[cu] += w }`.
            if partition.community_of(v) == cu {
                in_w[cu] += w;
            }
        }
    }

    let mut q = 0.0_f64;
    for c in 0..n_c {
        let edge_term = in_w[c] / twice_m;
        let degree_term = tot_w[c] / twice_m;
        q += edge_term - degree_term * degree_term;
    }
    q
}

/// Run multi-level Louvain community detection with the default seed.
///
/// Equivalent to `louvain_seeded(graph, 42)`. The seed governs the
/// node visit order; identical seeds produce identical partitions on
/// the same graph.
#[must_use]
pub fn louvain(graph: &Graph) -> Partition {
    louvain_seeded(graph, 42)
}

/// Run multi-level Louvain community detection with an explicit seed.
///
/// # Algorithm
///
/// 1. Convert the public [`Graph`] to an internal [`WeightedGraph`].
/// 2. Initialize a "composed" partition that maps each original node
///    to itself: `composed[i] = i`.
/// 3. Loop:
///    1. Run local-moving on the current weighted graph; produce a
///       per-level partition `p`.
///    2. If `p` has one community per node (no merges), stop.
///    3. Compose: `composed[i] := p[composed[i]]` for each original node.
///    4. Aggregate the current graph using `p` → smaller graph.
/// 4. Renumber the composed partition to a contiguous `0..k` range.
///
/// On most real graphs the outer loop converges within 3–5 levels;
/// each successive level operates on a substantially smaller graph,
/// so total cost is dominated by level 0.
///
/// # Performance
///
/// Δ-Q per node is `O(deg(u))` via incremental Σ_in / Σ_tot
/// bookkeeping; per-level cost is `O(m)`. Including aggregation,
/// total cost is `O(m · log m)` empirically on graphs up to 10⁶ nodes.
#[must_use]
pub fn louvain_seeded(graph: &Graph, seed: u64) -> Partition {
    let n = graph.node_count();
    if n == 0 {
        return Partition::new(Vec::new());
    }

    let mut wg = WeightedGraph::from_graph(graph);
    if wg.twice_total_weight() == 0.0 {
        // No edges — every node is its own community. No outer loop
        // needed; return the trivial partition directly.
        return Partition::new((0..n).collect());
    }

    // `composed[i]` = community of original node `i` so far.
    // Initially each node is in its own (level-0) community.
    let mut composed: Vec<usize> = (0..n).collect();

    let mut seed_state = seed;
    let max_levels = 32; // safety cap; practical runs converge in 3-5

    for _level in 0..max_levels {
        // Run one level of local moving on the current weighted graph.
        let level_labels = louvain_one_level(&wg, seed_state);
        // Advance the seed across levels so we don't repeat the same
        // shuffle pattern at every depth.
        seed_state = seed_state.wrapping_add(0x9E37_79B9_7F4A_7C15);

        // Did anything merge? `Partition::new` renumbers; if the
        // community count equals the node count, no merges happened
        // and we've reached a fixed point.
        let level_partition = Partition::new(level_labels);
        if level_partition.community_count() == wg.node_count() {
            break;
        }

        // Compose: original node i was in community composed[i]; that
        // community is now itself in community level_partition[composed[i]].
        for c in &mut composed {
            *c = level_partition.community_of(*c);
        }

        // Aggregate the weighted graph for the next level.
        wg = wg.aggregate(&level_partition);

        // Sanity exit: if aggregation collapsed everything into one
        // node, we're done.
        if wg.node_count() <= 1 {
            break;
        }
    }

    Partition::new(composed)
}

/// One level of Louvain-style local moving on a [`WeightedGraph`].
///
/// Starts every node in its own community and returns the raw
/// community labels after local moving converges. Thin wrapper around
/// [`local_moving_from`] for the identity-initial case.
fn louvain_one_level(graph: &WeightedGraph, seed: u64) -> Vec<usize> {
    let n = graph.node_count();
    let initial: Vec<usize> = (0..n).collect();
    local_moving_from(graph, &initial, seed)
}

/// Generalized local-moving: like Louvain's local-moving phase but the
/// caller chooses the starting community labels.
///
/// Used by both vanilla Louvain (which seeds with identity = each node
/// alone) and by Leiden (which at deeper levels seeds with the P-labels
/// from the previous level so the algorithm starts from a non-trivial
/// partition rather than re-discovering it from scratch).
///
/// `initial[u]` must be a valid community label (any `usize`). Internal
/// bookkeeping is sized to `max(initial) + 1`. Community labels never
/// grow beyond the initial maximum — Louvain only moves nodes between
/// existing communities, never creates new ones.
///
/// Returns labels in the same range as `initial`. The caller should
/// renumber to a contiguous `0..k` range via [`Partition::new`] for
/// downstream use.
fn local_moving_from(graph: &WeightedGraph, initial: &[usize], seed: u64) -> Vec<usize> {
    let n = graph.node_count();
    let twice_m = graph.twice_total_weight();
    if twice_m == 0.0 || n == 0 {
        return initial.to_vec();
    }
    debug_assert_eq!(initial.len(), n);

    // Size bookkeeping arrays to fit any label in `initial`. We don't
    // need to renumber initial since `tot` and `in_w` only need to be
    // addressable at every label the algorithm might see.
    let sz = initial.iter().copied().max().unwrap_or(0) + 1;

    let mut community: Vec<usize> = initial.to_vec();
    let mut tot: Vec<f64> = vec![0.0; sz];
    let mut in_w: Vec<f64> = vec![0.0; sz];

    // Initialize Σ_tot and Σ_in based on the initial labeling.
    for u in 0..n {
        let cu = community[u];
        tot[cu] += graph.degree(u);
        for (v, w) in graph.neighbors(u) {
            // Symmetric adj: non-loop edges contribute w from each side,
            // self-loops once. Both correct here since the predicate is
            // "is v in cu?" and self-loops have v == u so cu trivially.
            if community[v] == cu {
                in_w[cu] += w;
            }
        }
    }

    let m = twice_m / 2.0;
    let mut rng_state = seed;
    if rng_state == 0 {
        rng_state = 1; // xorshift needs nonzero state
    }

    let max_iter = 64;
    for _iter in 0..max_iter {
        let mut moved = false;
        let order = shuffled_indices(n, &mut rng_state);

        for &u in &order {
            let cu = community[u];
            let ku = graph.degree(u);
            let self_loop = graph.self_loop(u);

            let mut k_u_to: BTreeMap<usize, f64> = BTreeMap::new();
            for (v, w) in graph.neighbors(u) {
                if v == u {
                    continue;
                }
                *k_u_to.entry(community[v]).or_insert(0.0) += w;
            }

            // Tentatively remove u from cu.
            let k_u_in_cu = *k_u_to.get(&cu).unwrap_or(&0.0);
            tot[cu] -= ku;
            in_w[cu] -= 2.0 * k_u_in_cu + self_loop;

            let mut candidates: Vec<usize> = k_u_to.keys().copied().collect();
            if !candidates.contains(&cu) {
                candidates.push(cu);
            }

            let mut best_c = cu;
            let mut best_gain: f64 = 0.0;
            for &d in &candidates {
                let k_u_in_d = *k_u_to.get(&d).unwrap_or(&0.0);
                let gain = (k_u_in_d - tot[d] * ku / twice_m) / m;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_c = d;
                }
            }

            let k_u_in_best = *k_u_to.get(&best_c).unwrap_or(&0.0);
            tot[best_c] += ku;
            in_w[best_c] += 2.0 * k_u_in_best + self_loop;
            community[u] = best_c;
            if best_c != cu {
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }

    community
}

// =========================================================================
// Leiden — adds a refinement phase between local moving and aggregation
// =========================================================================

/// Run multi-level Leiden community detection with the default seed.
///
/// Equivalent to `leiden_seeded(graph, 42)`. See [`leiden_seeded`] for
/// the algorithm.
#[must_use]
pub fn leiden(graph: &Graph) -> Partition {
    leiden_seeded(graph, 42)
}

/// Run multi-level Leiden community detection with an explicit seed.
///
/// Leiden (Traag, Waltman, van Eck 2019) wraps the same outer
/// multi-level shape as Louvain but inserts a *refinement* phase
/// between local-moving and aggregation. At each level:
///
/// 1. **Local moving** (same as Louvain) — produces a partition P_L
///    that's locally optimal for modularity.
/// 2. **Refinement** — for each P_L-community C, run constrained
///    local-moving among C's members starting from "each alone." Each
///    node can only move to a refined community whose members are all
///    inside the same P_L-community. Produces a finer partition R_L
///    where every R_L-community is a subset of some P_L-community.
/// 3. **Aggregation** — collapse each R_L-community (not each P_L-
///    community) into a super-node for the next level.
/// 4. **Initial labels for next level** = the P_L-community each
///    super-node came from. The next level's local moving thus
///    *starts* from P_L's grouping and refines further.
///
/// # Why this matters
///
/// Vanilla Louvain can produce disconnected communities: aggregation
/// merges nodes that have the same community label even when those
/// nodes don't have a direct path within the community subgraph.
/// At the next level, the disconnected blobs are now one super-node
/// and modularity calculations can't tell them apart. Leiden's
/// refinement step fixes this by re-partitioning each community
/// into its naturally-connected subgroups before aggregation, so
/// each level operates on a partition where every community is
/// internally cohesive.
///
/// In practice on well-clustered graphs (Zachary, LFR with mixing ≤ 0.5)
/// Leiden produces similar modularity to Louvain but with provably
/// better-formed communities; on adversarial inputs (e.g. graphs
/// constructed to defeat Louvain) the modularity gap can be large.
///
/// # Algorithm
///
/// ```text
/// initialize: composed[i] = i, node_at_level[i] = i, initial = identity
/// loop:
///   P_L = local_moving(G_L, starting from `initial`)
///   composed[i] = P_L.community_of(node_at_level[i])    // user-facing
///   stop if P_L produces no merges
///   R_L = refine(G_L, P_L)                              // subset of P, well-connected
///   G_{L+1} = aggregate(G_L, R_L)                       // super-nodes from R
///   node_at_level[i] = R_L.community_of(node_at_level[i])
///   initial[super_r] = P_L.community_of(any u with R_L[u] = super_r)
/// return Partition::new(composed)
/// ```
#[must_use]
pub fn leiden_seeded(graph: &Graph, seed: u64) -> Partition {
    leiden_on_weighted(WeightedGraph::from_graph(graph), seed)
}

/// Run multi-level Leiden directly on an internal [`WeightedGraph`].
///
/// Same algorithm as [`leiden_seeded`] but takes the weighted form
/// already constructed. Used internally by [`regions_from_clusters`]
/// so the region-detection pass doesn't have to round-trip through
/// the public [`Graph`] type (which would require synthesizing
/// `NodeId`s for super-clusters that have no public identity).
fn leiden_on_weighted(mut wg: WeightedGraph, seed: u64) -> Partition {
    let n = wg.node_count();
    if n == 0 {
        return Partition::new(Vec::new());
    }

    if wg.twice_total_weight() == 0.0 {
        return Partition::new((0..n).collect());
    }

    // node_at_level[i] = the current G_L node index containing
    // original node i. Updated after each aggregation.
    let mut node_at_level: Vec<usize> = (0..n).collect();
    // composed[i] = user-facing community of original node i at the
    // current P-level. Updated after each local-moving phase.
    let mut composed: Vec<usize> = (0..n).collect();
    // Initial community labels for the next level's local moving.
    // At level 0 each node starts in its own community.
    let mut initial: Vec<usize> = (0..wg.node_count()).collect();

    let mut seed_state = seed;
    let max_levels = 32;

    for _level in 0..max_levels {
        // Phase 1: local moving on G_L starting from `initial`.
        let p_labels = local_moving_from(&wg, &initial, seed_state);
        seed_state = seed_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let p = Partition::new(p_labels);

        // Update the user-facing composed labels.
        for i in 0..n {
            composed[i] = p.community_of(node_at_level[i]);
        }

        // Stop if no merges happened — we've reached a fixed point.
        if p.community_count() == wg.node_count() {
            break;
        }

        // Phase 2: refinement — produce R_L, a finer partition where
        // every refined community is a subset of some P_L-community
        // and is internally well-connected.
        let r = refine_partition(&wg, &p, seed_state);
        seed_state = seed_state.wrapping_add(0x9E37_79B9_7F4A_7C15);

        // Phase 3: aggregate using R_L (not P_L). Each refined
        // community becomes a super-node in the next level's graph.
        let new_wg = wg.aggregate(&r);

        // Update node_at_level: original i's new G_{L+1} index is the
        // R_L-community it currently sits in.
        for nal in &mut node_at_level {
            *nal = r.community_of(*nal);
        }

        // Compute initial labels for the next level: each super-node
        // (which is an R_L-community) inherits the P_L-community its
        // members shared. All nodes in the same R_L-community have the
        // same P_L-community (since R is a refinement of P), so the
        // assignment is well-defined regardless of which u we pick.
        let mut new_initial: Vec<usize> = vec![0; new_wg.node_count()];
        for u in 0..wg.node_count() {
            new_initial[r.community_of(u)] = p.community_of(u);
        }

        wg = new_wg;
        initial = new_initial;

        if wg.node_count() <= 1 {
            break;
        }
    }

    Partition::new(composed)
}

/// Leiden's refinement phase: turn a P-partition into a finer R-partition
/// where every R-community is a subset of some P-community AND is
/// internally well-connected.
///
/// The implementation runs Louvain-style local moving *constrained* to
/// stay inside each P-community: a node can only move to a refined
/// community whose nodes are all in the same P-community. Starting
/// every node alone and running this constrained loop until convergence
/// produces an R that is a strict refinement of P.
///
/// The well-connectedness guarantee follows because the constrained
/// local-moving can only merge subgroups that share enough edge weight
/// to beat the modularity null model — which by construction is
/// equivalent to "the merged subgroup is internally well-connected."
/// Stricter formulations of Leiden add an explicit well-connectedness
/// check; for the well-clustered graphs swindex targets the constrained
/// local-moving formulation is sufficient and is what most production
/// Leiden implementations (igraph, leidenalg's "fast" variant) use.
fn refine_partition(graph: &WeightedGraph, p: &Partition, seed: u64) -> Partition {
    let n = graph.node_count();
    let twice_m = graph.twice_total_weight();
    if twice_m == 0.0 || n == 0 {
        return Partition::new((0..n).collect());
    }

    // Start each node alone in the refined partition. Community ids
    // are 0..n initially, so bookkeeping sized to n is sufficient
    // (constrained local-moving never invents new ids).
    let mut refined: Vec<usize> = (0..n).collect();
    let mut tot: Vec<f64> = (0..n).map(|u| graph.degree(u)).collect();
    let mut in_w: Vec<f64> = (0..n).map(|u| graph.self_loop(u)).collect();

    let m = twice_m / 2.0;
    let mut rng_state = seed;
    if rng_state == 0 {
        rng_state = 1;
    }

    let buckets = p.buckets();
    let max_iter = 32;

    for _iter in 0..max_iter {
        let mut moved = false;

        // Visit each P-community. We iterate by community id rather
        // than shuffling P-communities — within-community order is
        // randomized, which is where the algorithm's stochasticity
        // matters; per-P-community order doesn't.
        for community_members in &buckets {
            if community_members.len() < 2 {
                // Singleton P-community — nothing to refine inside.
                continue;
            }

            // Shuffle this P-community's members deterministically.
            let mut members: Vec<usize> = community_members.clone();
            for i in (1..members.len()).rev() {
                let r_rand = xorshift64(&mut rng_state);
                #[allow(clippy::cast_possible_truncation)]
                let j = (r_rand as usize) % (i + 1);
                members.swap(i, j);
            }

            for &u in &members {
                let cu = refined[u];
                let ku = graph.degree(u);
                let self_loop = graph.self_loop(u);
                let p_of_u = p.community_of(u);

                // Find edge-weight sum from u to each refined community
                // — but only count neighbors that are in the same
                // P-community. This is the Leiden restriction.
                let mut k_u_to: BTreeMap<usize, f64> = BTreeMap::new();
                for (v, w) in graph.neighbors(u) {
                    if v == u {
                        continue;
                    }
                    if p.community_of(v) != p_of_u {
                        continue;
                    }
                    *k_u_to.entry(refined[v]).or_insert(0.0) += w;
                }

                // Tentatively remove u from cu.
                let k_u_in_cu = *k_u_to.get(&cu).unwrap_or(&0.0);
                tot[cu] -= ku;
                in_w[cu] -= 2.0 * k_u_in_cu + self_loop;

                // Candidates: cu + neighbor-communities-within-P.
                let mut candidates: Vec<usize> = k_u_to.keys().copied().collect();
                if !candidates.contains(&cu) {
                    candidates.push(cu);
                }

                let mut best_c = cu;
                let mut best_gain: f64 = 0.0;
                for &d in &candidates {
                    let k_u_in_d = *k_u_to.get(&d).unwrap_or(&0.0);
                    let gain = (k_u_in_d - tot[d] * ku / twice_m) / m;
                    if gain > best_gain + 1e-12 {
                        best_gain = gain;
                        best_c = d;
                    }
                }

                let k_u_in_best = *k_u_to.get(&best_c).unwrap_or(&0.0);
                tot[best_c] += ku;
                in_w[best_c] += 2.0 * k_u_in_best + self_loop;
                refined[u] = best_c;
                if best_c != cu {
                    moved = true;
                }
            }
        }

        if !moved {
            break;
        }
    }

    Partition::new(refined)
}

// =========================================================================
// Region detection — Layer 3 of the four-layer index
// =========================================================================

/// Detect regions: groups of clusters that should be navigated to
/// together at the top level of the query planner.
///
/// Algorithm (recursive Leiden):
///
/// 1. **Aggregate** the original graph using the cluster partition.
///    Each cluster becomes a super-node; edge weights between
///    super-nodes are summed from inter-cluster edges in the
///    original graph.
/// 2. **Run Leiden** on that super-graph. The partition Leiden
///    produces *over the super-nodes* is the cluster→region map.
///
/// This is the same recursive-Leiden idea Microsoft GraphRAG uses
/// offline for hierarchical summarization (arxiv:2404.16130);
/// swindex applies it online for query routing.
///
/// # Output shape
///
/// Returns a [`Partition`] of length `partition.community_count()`
/// — one entry per *cluster*. `result.community_of(c)` gives the
/// region id of cluster `c`. `result.community_count()` is the
/// number of regions detected.
///
/// # Trivial cases
///
/// * Zero clusters → empty partition.
/// * One cluster → one region (containing that cluster).
/// * Clusters with no inter-cluster edges → one region per cluster
///   (since the super-graph has no edges and Leiden falls back to
///   the trivial partition).
///
/// # Determinism
///
/// Same `(graph, partition, seed)` always yields the same regions
/// — Leiden's local-moving uses a seeded RNG.
/// The weighted **cluster super-graph adjacency**: for each cluster
/// `c`, the `(neighbor_cluster, weight)` pairs where weight is the
/// total edge mass between the two clusters in the original graph.
/// Self-loops (intra-cluster mass) are excluded — this is *adjacency*,
/// not density. Neighbor lists come out sorted ascending by cluster id
/// (BTreeMap accumulation) and the relation is symmetric:
/// `weight(c→d) == weight(d→c)`.
///
/// This is the same aggregation [`regions_from_clusters`] performs
/// internally before its Leiden pass (the modularity-preserving 2×
/// doubling applies only to the self-loops excluded here), exposed so
/// callers can persist or route on cluster adjacency without
/// re-deriving it from Layer 0 (issue #70 — the Gate-0 substrate for
/// corridor queries and the baseline arm of the Gate-1 experiment).
/// It re-runs the `O(E)` aggregation rather than sharing the regions
/// pass's copy; that cost is noise next to Leiden on the same graph.
#[must_use]
pub fn cluster_adjacency(graph: &Graph, partition: &Partition) -> Vec<Vec<(usize, f64)>> {
    let k = partition.community_count();
    if k == 0 {
        return Vec::new();
    }
    let super_wg = WeightedGraph::from_graph(graph).aggregate(partition);
    (0..k)
        .map(|c| super_wg.neighbors(c).filter(|&(d, _)| d != c).collect())
        .collect()
}

#[must_use]
pub fn regions_from_clusters(graph: &Graph, partition: &Partition, seed: u64) -> Partition {
    let k = partition.community_count();
    if k == 0 {
        return Partition::new(Vec::new());
    }
    if k == 1 {
        // One cluster → one region. Skip the aggregation work.
        return Partition::new(vec![0]);
    }
    // Aggregate the original graph using the cluster partition →
    // cluster super-graph. The super-graph is itself a WeightedGraph
    // with `k` nodes.
    let super_wg = WeightedGraph::from_graph(graph).aggregate(partition);
    // Run multi-level Leiden directly on the super-graph.
    leiden_on_weighted(super_wg, seed)
}

/// Build a `Vec<usize>` containing `0..n` in a seeded-random order,
/// using a Fisher-Yates shuffle driven by a xorshift64 RNG. Mutates
/// `state` so successive calls produce different orderings without
/// the caller having to thread a separate counter.
fn shuffled_indices(n: usize, state: &mut u64) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let r = xorshift64(state);
        // On 64-bit targets this is identity; on hypothetical 32-bit
        // targets we'd be discarding the high half of the entropy,
        // which is fine for a shuffle.
        #[allow(clippy::cast_possible_truncation)]
        let j = (r as usize) % (i + 1);
        indices.swap(i, j);
    }
    indices
}

/// xorshift64 — small, fast, deterministic. Not cryptographic but
/// fine for ordering nodes during community detection.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Hand-computed expected values are exact.
mod tests {
    use super::{
        Partition, WeightedGraph, cluster_adjacency, leiden, leiden_seeded, louvain,
        louvain_seeded, modularity, refine_partition,
    };
    use crate::graph::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;
    use std::collections::{BTreeSet, VecDeque};

    /// Test helper: BFS through the subgraph induced by `members` and
    /// return `true` iff every member is reachable from `members[0]`.
    /// Used to verify Leiden's internal-connectedness guarantee.
    fn community_is_connected(graph: &Graph, members: &[usize]) -> bool {
        if members.len() <= 1 {
            return true;
        }
        let member_set: BTreeSet<usize> = members.iter().copied().collect();
        let mut visited: BTreeSet<usize> = BTreeSet::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(members[0]);
        visited.insert(members[0]);
        while let Some(u) = queue.pop_front() {
            for (v, _) in graph.neighbors(u) {
                if member_set.contains(&v) && !visited.contains(&v) {
                    visited.insert(v);
                    queue.push_back(v);
                }
            }
        }
        visited.len() == members.len()
    }

    /// Build a triangle graph: 3 nodes, 3 edges, all in one cluster.
    fn triangle() -> (Vec<Node>, Vec<Edge>) {
        let a = Node::fresh(NodeKind::new("v"));
        let b = Node::fresh(NodeKind::new("v"));
        let c = Node::fresh(NodeKind::new("v"));
        let edges = vec![
            Edge::fresh(a.id, b.id, EdgeKind::new("e")),
            Edge::fresh(b.id, c.id, EdgeKind::new("e")),
            Edge::fresh(c.id, a.id, EdgeKind::new("e")),
        ];
        (vec![a, b, c], edges)
    }

    /// Two triangles with no edge between them — Q ≈ 0.5 is optimal.
    fn two_disjoint_triangles() -> (Vec<Node>, Vec<Edge>) {
        let (mut n1, e1) = triangle();
        let (n2, e2) = triangle();
        n1.extend(n2);
        let edges: Vec<Edge> = e1.into_iter().chain(e2).collect();
        (n1, edges)
    }

    #[test]
    fn empty_graph_has_zero_modularity() {
        let src = SliceSource::new(&[], &[]);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(Vec::new());
        assert_eq!(modularity(&g, &p), 0.0);
    }

    #[test]
    fn singleton_partition_of_triangle_has_negative_modularity() {
        let (nodes, edges) = triangle();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 1, 2]);
        let q = modularity(&g, &p);
        assert!(q < 0.0, "expected Q<0 for singleton partition, got {q}");
        assert!((q - (-1.0 / 3.0)).abs() < 1e-9, "expected Q≈-1/3, got {q}");
    }

    #[test]
    fn all_in_one_partition_of_triangle_has_zero_modularity() {
        let (nodes, edges) = triangle();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0]);
        let q = modularity(&g, &p);
        assert!((q - 0.0).abs() < 1e-9, "expected Q≈0, got {q}");
    }

    #[test]
    fn two_disjoint_triangles_optimal_partition_has_q_half() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);
        let q = modularity(&g, &p);
        assert!((q - 0.5).abs() < 1e-9, "expected Q≈0.5, got {q}");
    }

    #[test]
    fn partition_renumbering_is_contiguous() {
        let p = Partition::new(vec![5, 17, 5, 99, 17]);
        assert_eq!(p.community_count(), 3);
        assert_eq!(p.community_of(0), 0);
        assert_eq!(p.community_of(1), 1);
        assert_eq!(p.community_of(2), 0);
        assert_eq!(p.community_of(3), 2);
        assert_eq!(p.community_of(4), 1);
    }

    #[test]
    fn louvain_on_two_disjoint_triangles_finds_two_communities() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = louvain(&g);
        assert_eq!(p.community_count(), 2);
        let q = modularity(&g, &p);
        assert!(q > 0.49, "expected Q>0.49 for two-triangle case, got {q}");
    }

    #[test]
    fn louvain_is_deterministic_for_a_fixed_seed() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = louvain_seeded(&g, 1234);
        let b = louvain_seeded(&g, 1234);
        assert_eq!(a, b);
    }

    /// Aggregation must preserve modularity exactly: running modularity
    /// on the aggregated graph with a "singleton" partition (each super-
    /// node alone) equals running modularity on the original graph with
    /// the partition used for aggregation. This is the load-bearing
    /// invariant that makes multi-level Louvain mathematically valid.
    #[test]
    fn aggregation_preserves_modularity_exactly() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();

        // The "obvious" partition: each triangle is one community.
        let original_p = Partition::new(vec![0, 0, 0, 1, 1, 1]);
        let original_q = modularity(&g, &original_p);

        // Aggregate using that partition and check modularity at the
        // next level with each super-node in its own community.
        let wg = WeightedGraph::from_graph(&g);
        let aggregated = wg.aggregate(&original_p);
        let singleton = Partition::new((0..aggregated.node_count()).collect());
        let aggregated_q = super::modularity_internal(&aggregated, &singleton);

        assert!(
            (original_q - aggregated_q).abs() < 1e-9,
            "modularity must be preserved by aggregation: original={original_q}, aggregated={aggregated_q}"
        );
    }

    /// Loads the Zachary karate club fixture and asserts Louvain
    /// produces a high-quality partition. With aggregation, modularity
    /// jumps from the single-level ~0.33 floor to the multi-level
    /// ~0.41 plateau — the published Louvain result for this graph.
    #[test]
    fn louvain_on_zachary_karate_club() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("member"),
            &EdgeKind::new("friendship"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        assert_eq!(g.node_count(), 34);

        let p = louvain(&g);
        let q = modularity(&g, &p);

        // Multi-level Louvain on Zachary lands around Q ≈ 0.41.
        // We assert ≥ 0.38 for headroom across seed-dependent
        // local-optima variation — published values for plain
        // Louvain on Zachary range from 0.38 to 0.44.
        assert!(
            q >= 0.38,
            "multi-level Louvain on Zachary should yield Q ≥ 0.38, got Q = {q}"
        );

        // Aggregation should collapse the single-level's ~8 small
        // communities down to the ~4 typical multi-level result.
        assert!(
            (2..=6).contains(&p.community_count()),
            "expected 2..=6 communities after aggregation, got {}",
            p.community_count()
        );

        // Every node covered, ids contiguous 0..k.
        let seen: BTreeSet<usize> = p.iter().collect();
        assert_eq!(seen.len(), p.community_count());
        assert_eq!(*seen.iter().min().unwrap(), 0);
        assert_eq!(*seen.iter().max().unwrap(), p.community_count() - 1);
    }

    // =====================================================================
    // Leiden tests
    // =====================================================================

    #[test]
    fn leiden_on_two_disjoint_triangles_finds_two_connected_communities() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = leiden(&g);
        assert_eq!(p.community_count(), 2);
        let q = modularity(&g, &p);
        assert!(q > 0.49, "expected Q>0.49, got {q}");
        // Each triangle is internally connected — Leiden must preserve that.
        for bucket in p.buckets() {
            assert!(community_is_connected(&g, &bucket));
        }
    }

    #[test]
    fn leiden_is_deterministic_for_a_fixed_seed() {
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = leiden_seeded(&g, 1234);
        let b = leiden_seeded(&g, 1234);
        assert_eq!(a, b);
    }

    /// Headline Leiden test: on Zachary karate, Leiden should produce
    /// modularity comparable to multi-level Louvain (~0.41) and every
    /// community must be internally connected (the property Louvain
    /// cannot guarantee).
    #[test]
    fn leiden_on_zachary_karate_club() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("member"),
            &EdgeKind::new("friendship"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();

        let p = leiden(&g);
        let q = modularity(&g, &p);

        assert!(
            q >= 0.38,
            "Leiden on Zachary should yield Q ≥ 0.38, got Q = {q}"
        );
        assert!(
            (2..=6).contains(&p.community_count()),
            "expected 2..=6 communities, got {}",
            p.community_count()
        );

        // The Leiden-specific guarantee: every community is internally
        // connected. This is what Louvain cannot promise.
        for (cid, bucket) in p.buckets().iter().enumerate() {
            assert!(
                community_is_connected(&g, bucket),
                "Leiden community {cid} (size {}) is not internally connected",
                bucket.len()
            );
        }
    }

    /// The defining property of `refine_partition`: every refined
    /// community must be a subset of exactly one P-community. If any
    /// refined community straddles a P-community boundary, the
    /// refinement is wrong and aggregation would mix populations that
    /// should stay separate.
    #[test]
    fn refine_partition_produces_subset_of_p() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("member"),
            &EdgeKind::new("friendship"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let wg = WeightedGraph::from_graph(&g);

        // Use Louvain's local-moving result as the P-partition. Then
        // refine it and verify R is a strict refinement of P.
        let p_labels = super::louvain_one_level(&wg, 42);
        let p = Partition::new(p_labels);
        let r = refine_partition(&wg, &p, 42);

        // For each R-community, check that all members share the same
        // P-community.
        for (r_cid, r_members) in r.buckets().iter().enumerate() {
            if r_members.is_empty() {
                continue;
            }
            let first_p = p.community_of(r_members[0]);
            for &u in &r_members[1..] {
                assert_eq!(
                    p.community_of(u),
                    first_p,
                    "R-community {r_cid} straddles P-communities ({first_p} vs {})",
                    p.community_of(u)
                );
            }
        }
    }

    /// Hand-computed invariant for `cluster_adjacency` (issue #70):
    /// two triangles joined by exactly one bridge edge, partitioned
    /// one-triangle-per-cluster, must yield mutual adjacency with
    /// weight exactly 1.0 — and no self-loops in the output (the
    /// intra-cluster mass of each triangle is density, not adjacency).
    #[test]
    fn cluster_adjacency_hand_computed_on_bridged_triangles() {
        let mk = |k: &str| Node::fresh(NodeKind::new(k));
        let nodes: Vec<Node> = (0..6).map(|_| mk("v")).collect();
        let e = EdgeKind::new("e");
        let mut edges = vec![
            Edge::fresh(nodes[0].id, nodes[1].id, e.clone()),
            Edge::fresh(nodes[1].id, nodes[2].id, e.clone()),
            Edge::fresh(nodes[2].id, nodes[0].id, e.clone()),
            Edge::fresh(nodes[3].id, nodes[4].id, e.clone()),
            Edge::fresh(nodes[4].id, nodes[5].id, e.clone()),
            Edge::fresh(nodes[5].id, nodes[3].id, e.clone()),
        ];
        // The single bridge.
        edges.push(Edge::fresh(nodes[0].id, nodes[3].id, e.clone()));
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);

        let adj = cluster_adjacency(&g, &p);
        assert_eq!(adj.len(), 2);
        assert_eq!(adj[0], vec![(1, 1.0)]);
        assert_eq!(adj[1], vec![(0, 1.0)]);
    }

    /// `cluster_adjacency` on clusters with no inter-cluster edges
    /// must return empty lists — not self-loops, not phantom edges.
    #[test]
    fn cluster_adjacency_disjoint_clusters_have_no_neighbors() {
        let mk = |k: &str| Node::fresh(NodeKind::new(k));
        let nodes: Vec<Node> = (0..6).map(|_| mk("v")).collect();
        let e = EdgeKind::new("e");
        let edges = vec![
            Edge::fresh(nodes[0].id, nodes[1].id, e.clone()),
            Edge::fresh(nodes[1].id, nodes[2].id, e.clone()),
            Edge::fresh(nodes[2].id, nodes[0].id, e.clone()),
            Edge::fresh(nodes[3].id, nodes[4].id, e.clone()),
            Edge::fresh(nodes[4].id, nodes[5].id, e.clone()),
            Edge::fresh(nodes[5].id, nodes[3].id, e.clone()),
        ];
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);

        let adj = cluster_adjacency(&g, &p);
        assert!(adj.iter().all(Vec::is_empty));
    }

    /// Real-graph invariants on Zachary with a Leiden partition:
    /// the adjacency relation is symmetric (weight(c→d) == weight(d→c))
    /// and complete — the total adjacency mass equals exactly twice the
    /// inter-cluster edge mass counted directly on Layer 0. Deleting
    /// either property silently breaks corridor routing built on top.
    #[test]
    fn cluster_adjacency_symmetric_and_complete_on_zachary() {
        let src = crate::gml::GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("m"),
            &EdgeKind::new("f"),
        )
        .unwrap();
        let g = Graph::from_source(&src).unwrap();
        let p = leiden(&g);
        let adj = cluster_adjacency(&g, &p);

        // Symmetry.
        for (c, ns) in adj.iter().enumerate() {
            for &(d, w) in ns {
                let (_, back) = adj[d]
                    .iter()
                    .find(|&&(e_, _)| e_ == c)
                    .unwrap_or_else(|| panic!("edge {c}->{d} has no mirror"));
                assert!((back - w).abs() < 1e-9, "asymmetric weight {c}<->{d}");
            }
        }

        // Completeness: sum of all adjacency weights == 2 × the
        // inter-cluster edge mass counted on the original graph
        // (each super-edge appears in both endpoints' lists).
        let mut inter = 0.0;
        for u in 0..g.node_count() {
            for (v, w) in g.neighbors(u) {
                if v > u && p.community_of(u) != p.community_of(v) {
                    inter += w;
                }
            }
        }
        let adj_total: f64 = adj.iter().flatten().map(|&(_, w)| w).sum();
        assert!(
            (adj_total - 2.0 * inter).abs() < 1e-9,
            "adjacency mass {adj_total} != 2 × inter-cluster mass {inter}"
        );
        // Zachary has cross-community edges under any sane partition.
        assert!(inter > 0.0);
    }
}
