//! Community detection on a [`Graph`].
//!
//! # What this module ships
//!
//! * [`Partition`] — a labeling of every node with a community id.
//! * [`modularity`] — the Newman-Girvan modularity of a partition, the
//!   standard quality score for community detection.
//! * [`louvain`] / [`louvain_seeded`] — the local-moving phase of the
//!   Louvain algorithm (Blondel et al. 2008). Produces a single-level
//!   partition with modularity > 0.3 on well-clustered graphs like
//!   Zachary's karate club.
//!
//! # Why Louvain first, not Leiden
//!
//! Louvain (2008) and Leiden (Traag 2019) share the same outer
//! structure — local moving, then community-aggregation, then recurse.
//! Leiden adds a *refinement* phase between the local moving and the
//! aggregation that guarantees every community is internally connected.
//! Without refinement (i.e., plain Louvain), it's possible — though
//! rare — for the algorithm to produce a "community" that's actually
//! two disconnected components glued together.
//!
//! For this PR we ship plain Louvain because:
//!
//! 1. It's the conceptual foundation Leiden builds on. Getting the
//!    modularity bookkeeping and the local-moving loop right here
//!    means the Leiden upgrade in the next PR is a smaller diff.
//! 2. On well-clustered fixture graphs (Zachary, LFR with μ ≤ 0.5),
//!    Louvain already produces partitions with modularity ≥ 0.4 — the
//!    disconnected-community pathology requires constructed
//!    adversarial inputs to surface.
//! 3. The next PR will upgrade to Leiden refinement *and* add the
//!    aggregation phase that lets the algorithm find multi-resolution
//!    structure. Layering both upgrades on top of a tested Louvain
//!    base is cleaner than mixing them.
//!
//! # Determinism
//!
//! The local-moving loop visits nodes in a random order — Louvain's
//! result depends on the visit order. We use a seeded xorshift RNG
//! so the partition is deterministic given a (graph, seed) pair. The
//! default [`louvain`] entry point uses seed `42`; tests rely on this
//! to assert stable partition counts and modularity values.

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
    let twice_m = graph.twice_total_weight();
    if twice_m == 0.0 {
        // Empty graph: no edges, no modularity to speak of. Convention
        // is to return 0 rather than NaN or +infinity.
        return 0.0;
    }

    let n_c = partition.community_count();
    // Σ_in for each community. Self-loops contribute once via the
    // graph's `self_loop` accessor; cross-edges contribute twice
    // through the symmetric adjacency.
    let mut in_w = vec![0.0_f64; n_c];
    let mut tot_w = vec![0.0_f64; n_c];

    for u in 0..graph.node_count() {
        let cu = partition.community_of(u);
        tot_w[cu] += graph.degree(u);
        for (v, w) in graph.neighbors(u) {
            // Symmetric adj: when u != v this loop runs twice for that
            // edge (once from u, once from v), each time contributing
            // w to in_w[cu] if both endpoints share the community —
            // which gives the standard "2 * within-edge-weight" total.
            // Self-loops only appear once in the adj list so they
            // contribute w to in_w[cu] exactly once.
            if partition.community_of(v) == cu {
                in_w[cu] += w;
            }
        }
    }

    let mut q = 0.0_f64;
    for c in 0..n_c {
        // The two-term modularity contribution from community c.
        let edge_term = in_w[c] / twice_m;
        let degree_term = tot_w[c] / twice_m;
        q += edge_term - degree_term * degree_term;
    }
    q
}

/// Run Louvain community detection with the default seed.
///
/// Equivalent to `louvain_seeded(graph, 42)`. The seed governs the
/// node visit order; identical seeds produce identical partitions on
/// the same graph.
#[must_use]
pub fn louvain(graph: &Graph) -> Partition {
    louvain_seeded(graph, 42)
}

/// Run Louvain community detection with an explicit seed.
///
/// The seed determines the order in which nodes are visited during
/// each local-moving pass. Different seeds can produce different
/// local optima — modularity values typically differ by < 0.01 across
/// seeds on real graphs.
///
/// Returns a [`Partition`] whose community labels are renumbered to a
/// contiguous `0..k` range.
///
/// # Algorithm
///
/// 1. Initialize every node in its own community.
/// 2. Repeat until no node moves in a full pass:
///    1. Visit every node in seeded-random order.
///    2. For each node `u`, consider moving it to the community of
///       each of its neighbors (plus its current community).
///    3. Pick the move with the largest positive Δ-Q and apply it.
/// 3. Renumber community labels to `0..k`.
///
/// This is the local-moving phase of Blondel et al. 2008. The
/// algorithm halts when no positive move exists; in practice this
/// happens within 5–30 iterations on graphs up to 10⁶ nodes.
///
/// # Performance
///
/// The Δ-Q evaluation per node is `O(deg(u))` thanks to per-community
/// `Σ_in` / `Σ_tot` bookkeeping maintained incrementally. Total
/// per-pass cost is `O(m)` where `m` is the edge count. The number
/// of passes is empirically small (≤ ~30) so total cost is roughly
/// `O(m · log m)` on practical inputs.
#[must_use]
pub fn louvain_seeded(graph: &Graph, seed: u64) -> Partition {
    let n = graph.node_count();
    if n == 0 {
        return Partition::new(Vec::new());
    }

    let twice_m = graph.twice_total_weight();
    if twice_m == 0.0 {
        // No edges — every node is its own community. Return that
        // partition directly; there's no work to do.
        return Partition::new((0..n).collect());
    }

    // Per-node state: which community is it currently in.
    let mut community: Vec<usize> = (0..n).collect();

    // Per-community state: Σ_in and Σ_tot. Indexed by community id,
    // which initially equals the node index since every node is in
    // its own community. As nodes move, some entries go to zero —
    // we don't compact mid-run because the index would shift; we
    // renumber at the end via `Partition::new`.
    //
    // Initial values:
    //   - Σ_tot[u] = degree(u)             — community {u} has u's degree
    //   - Σ_in[u] = self_loop_weight(u)    — only the self-loop is "within"
    let mut tot: Vec<f64> = (0..n).map(|u| graph.degree(u)).collect();
    let mut in_w: Vec<f64> = (0..n).map(|u| graph.self_loop(u)).collect();

    // Constants pulled out of the inner loop.
    let m = twice_m / 2.0;
    let two_m_sq = twice_m * twice_m; // (2m)^2, the divisor in Δ-Q

    let mut rng_state = seed;
    if rng_state == 0 {
        rng_state = 1; // xorshift needs nonzero state
    }

    let max_iter = 64; // safety cap; practical runs converge in <30
    for _iter in 0..max_iter {
        let mut moved = false;

        // Visit nodes in a seeded random order. Order changes every
        // pass (we keep advancing rng_state) so we don't keep
        // re-attempting the same dead-end sequence.
        let order = shuffled_indices(n, &mut rng_state);

        for &u in &order {
            let cu = community[u];
            let ku = graph.degree(u);

            // Sum of edge weights from u to each neighboring community.
            // BTreeMap rather than HashMap so iteration is deterministic.
            let mut k_u_to: BTreeMap<usize, f64> = BTreeMap::new();
            // Self-loop weight from u to its current community — used
            // to correctly remove u from cu (the self-loop was in in_w[cu]
            // because cu was u's community).
            let self_loop = graph.self_loop(u);
            for (v, w) in graph.neighbors(u) {
                if v == u {
                    continue; // self-loop already tracked separately
                }
                *k_u_to.entry(community[v]).or_insert(0.0) += w;
            }

            // Tentatively remove u from cu. After this:
            //   - tot[cu] no longer includes u's degree
            //   - in_w[cu] no longer includes u's contributions
            // We'll re-add u to whichever community wins the search.
            let k_u_in_cu = *k_u_to.get(&cu).unwrap_or(&0.0);
            tot[cu] -= ku;
            in_w[cu] -= 2.0 * k_u_in_cu + self_loop;

            // Candidate communities to evaluate: u's current cu (so
            // "stay put" is always considered after removal) plus
            // every neighbor's community.
            //
            // We use a Vec for the candidate set rather than iterating
            // k_u_to directly so we can add `cu` exactly once even if
            // `cu` is also a key in `k_u_to` (which happens when u has
            // a neighbor in cu).
            let mut candidates: Vec<usize> = k_u_to.keys().copied().collect();
            if !candidates.contains(&cu) {
                candidates.push(cu);
            }

            // Find the best target. Treat "no move" as Δ-Q = 0 so we
            // only switch communities when a strictly positive gain
            // exists (small epsilon to avoid floating-point thrash).
            let mut best_c = cu;
            let mut best_gain: f64 = 0.0;
            for &d in &candidates {
                let k_u_in_d = *k_u_to.get(&d).unwrap_or(&0.0);
                // Δ-Q for inserting an isolated node u into community d:
                //   gain = k_u_in_d / m  -  Σ_tot_d * k_u / (2m * m)
                //        = (2 * k_u_in_d - Σ_tot_d * k_u / m) / twice_m
                // Rearranged to avoid two divisions in the hot loop:
                let gain = (k_u_in_d - tot[d] * ku / twice_m) / m;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_c = d;
                }
            }

            // Apply the move (which may be "back to cu" — that's fine,
            // the bookkeeping reverses cleanly).
            let k_u_in_best = *k_u_to.get(&best_c).unwrap_or(&0.0);
            tot[best_c] += ku;
            in_w[best_c] += 2.0 * k_u_in_best + self_loop;
            community[u] = best_c;
            if best_c != cu {
                moved = true;
            }
        }

        if !moved {
            // Converged — no node could improve Q by moving.
            break;
        }
    }

    // Use only the final community labels; the bookkeeping arrays (tot,
    // in_w) intentionally aren't returned — callers can recompute
    // modularity from the Partition + Graph if they want it.
    let _ = (tot, in_w, two_m_sq);

    Partition::new(community)
}

/// Build a `Vec<usize>` containing `0..n` in a seeded-random order,
/// using a Fisher-Yates shuffle driven by a xorshift64 RNG. Mutates
/// `state` so successive calls produce different orderings without
/// the caller having to thread a separate counter.
fn shuffled_indices(n: usize, state: &mut u64) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..n).collect();
    // Standard Fisher-Yates: for i from n-1 down to 1, swap with a
    // uniformly random index in [0, i].
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
    use super::{Partition, louvain, louvain_seeded, modularity};
    use crate::graph::Graph;
    use crate::node::{Edge, EdgeKind, Node, NodeKind};
    use crate::source::SliceSource;
    use std::collections::BTreeSet;

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

    /// Build a 6-node graph: two triangles with no edge between them.
    /// The "obvious" partition is {0,1,2} and {3,4,5}; modularity ≈ 0.5.
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
        // Convention: empty graph → Q = 0. Avoids NaN from the 0/0 in
        // the modularity formula.
        assert_eq!(modularity(&g, &p), 0.0);
    }

    #[test]
    fn singleton_partition_of_triangle_has_negative_modularity() {
        // Every node in its own community on a connected triangle:
        // edges are all "between" communities, so Q is strictly negative.
        // Specifically Q = 0 - 3 * (2/(2*3))^2 = -1/3 by direct calc.
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
        // All three nodes in one community: Q = 6/6 - (6/6)^2 = 0.
        // (Σ_in = 6 because each non-loop edge counts twice in symmetric
        // adj; Σ_tot = 6 = sum of degrees.)
        let (nodes, edges) = triangle();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0]);
        let q = modularity(&g, &p);
        assert!((q - 0.0).abs() < 1e-9, "expected Q≈0, got {q}");
    }

    #[test]
    fn two_disjoint_triangles_optimal_partition_has_q_half() {
        // Two triangles, no inter-cluster edges. The optimal partition
        // groups each triangle. Standard result: Q = 1/2.
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let p = Partition::new(vec![0, 0, 0, 1, 1, 1]);
        let q = modularity(&g, &p);
        assert!((q - 0.5).abs() < 1e-9, "expected Q≈0.5, got {q}");
    }

    #[test]
    fn partition_renumbering_is_contiguous() {
        // Partition::new must renumber arbitrary labels (5, 17, 5, 99,
        // 17) to a contiguous 0..k range (0, 1, 0, 2, 1).
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
        // The "easy" case: Louvain must recover the disconnected
        // triangles as separate communities. Modularity ≈ 0.5.
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
        // Same graph + same seed must yield the same partition every run.
        // Without this guarantee, downstream regression tests on cluster
        // counts/modularity become flaky.
        let (nodes, edges) = two_disjoint_triangles();
        let src = SliceSource::new(&nodes, &edges);
        let g = Graph::from_source(&src).unwrap();
        let a = louvain_seeded(&g, 1234);
        let b = louvain_seeded(&g, 1234);
        assert_eq!(a, b);
    }

    /// Loads the Zachary karate club fixture and asserts Louvain
    /// produces a useful partition on it. This is the headline test —
    /// the algorithm has to clear a modularity bar that random
    /// partitions can't reach.
    #[test]
    fn louvain_on_zachary_karate_club() {
        // Use the GmlSource path so we exercise the full chain:
        // GML on disk → Graph → Louvain → Partition → modularity.
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

        // Lenient sanity bound: real Louvain runs on Zachary land in
        // the 0.36–0.42 range. We assert ≥ 0.30 to leave headroom for
        // local-optima variation between seeds without becoming flaky.
        assert!(
            q >= 0.30,
            "Louvain on Zachary should yield Q ≥ 0.30, got Q = {q}"
        );

        // The expected number of communities is between 2 (the
        // original 1977 split) and ~5 (smaller subgroups Louvain often
        // finds). Anything outside [2, 8] means the algorithm degenerated.
        assert!(
            (2..=8).contains(&p.community_count()),
            "expected 2..=8 communities on Zachary, got {}",
            p.community_count()
        );

        // Every node must be covered exactly once, and community ids
        // must form a contiguous 0..k range (which Partition::new
        // guarantees, but we double-check here as a regression net).
        let seen: BTreeSet<usize> = p.iter().collect();
        assert_eq!(seen.len(), p.community_count());
        assert_eq!(*seen.iter().min().unwrap(), 0);
        assert_eq!(*seen.iter().max().unwrap(), p.community_count() - 1);
    }
}
