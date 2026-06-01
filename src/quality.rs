//! Quality metrics for comparing partitions of the same node set.
//!
//! # What this module ships
//!
//! [`nmi`] — Normalized Mutual Information between two
//! [`Partition`](crate::community::Partition)s. Measures how well two
//! community assignments agree, invariant under label permutation.
//!
//! # Why a separate module
//!
//! Quality metrics belong with the algorithm primitives that produce
//! the partitions (`community.rs`), not the persistence layer
//! (`index.rs`). Splitting them out keeps `community.rs` focused on
//! Louvain/Leiden mechanics and gives integration tests a small,
//! self-contained import surface (`use swindex::nmi;`).
//!
//! The first consumer is the planted-partition correctness suite
//! (`tests/clustering_quality.rs`, issue #41). Future consumers:
//!
//! * Phase-2+ Ada-IVF correctness — "does an incremental update
//!   degrade NMI vs the from-scratch baseline beyond ε?" (issue #27)
//! * Adversarial fixtures from Traag et al. (issue #30) — same NMI
//!   API, different planted partitions.
//!
//! # Conventions
//!
//! * Natural log throughout. The normalization `2·I / (H_a + H_b)`
//!   makes the log base irrelevant.
//! * Returns a value in `[0.0, 1.0]`. Floating-point arithmetic can
//!   nudge tiny epsilons; callers should compare with `>= threshold`
//!   not `> threshold`.
//! * `0·log 0 = 0` by convention (the limit; skip the term).

use crate::community::Partition;

/// Normalized Mutual Information between two partitions of the same
/// node set.
///
/// Returns a value in `[0.0, 1.0]`:
/// * `1.0` — partitions agree perfectly (up to label permutation).
/// * `0.0` — partitions are statistically independent.
///
/// Uses the standard arithmetic-mean normalization:
/// `NMI = 2·I(A,B) / (H(A) + H(B))`. Symmetric in its arguments.
///
/// # Degenerate cases
///
/// * If both partitions are trivial (one community covering every
///   node), `H(A) = H(B) = 0` and we return `1.0` — they agree.
/// * If exactly one partition is trivial, the non-trivial side carries
///   all the information about the joint distribution but the trivial
///   side carries none, so `I = 0` and we return `0.0`.
///
/// # Panics
///
/// Panics if the two partitions cover different numbers of nodes —
/// NMI is only meaningful on a shared node set, and silently
/// truncating to the shorter would hide bugs.
///
/// # Example
///
/// ```
/// use swindex::{nmi, Partition};
///
/// // Identical partitions agree perfectly.
/// let a = Partition::new(vec![0, 0, 1, 1, 2, 2]);
/// let b = Partition::new(vec![0, 0, 1, 1, 2, 2]);
/// assert!((nmi(&a, &b) - 1.0).abs() < 1e-12);
///
/// // Same partition under relabeling — still NMI = 1.
/// let c = Partition::new(vec![5, 5, 9, 9, 2, 2]);
/// assert!((nmi(&a, &c) - 1.0).abs() < 1e-12);
/// ```
#[must_use]
pub fn nmi(a: &Partition, b: &Partition) -> f64 {
    let n = a.node_count();
    assert_eq!(
        n,
        b.node_count(),
        "nmi: partitions cover different numbers of nodes ({} vs {})",
        n,
        b.node_count(),
    );
    if n == 0 {
        // Vacuous case — both partitions cover nothing. By convention,
        // they trivially agree.
        return 1.0;
    }

    let ka = a.community_count();
    let kb = b.community_count();

    // Build the contingency table c[i][j] = |A_i ∩ B_j|.
    // ka × kb entries; for planted-partition tests this is tiny
    // (k_planted × k_detected, both single-digit usually).
    let mut contingency = vec![vec![0_usize; kb]; ka];
    for idx in 0..n {
        let ai = a.community_of(idx);
        let bj = b.community_of(idx);
        contingency[ai][bj] += 1;
    }

    // Marginals.
    let mut marg_a = vec![0_usize; ka];
    let mut marg_b = vec![0_usize; kb];
    for (i, row) in contingency.iter().enumerate() {
        for (j, &c_ij) in row.iter().enumerate() {
            marg_a[i] += c_ij;
            marg_b[j] += c_ij;
        }
    }

    // Avoid casting precision loss warnings: the contingency entries
    // are bounded by n which fits comfortably in f64 mantissa for any
    // realistic benchmark size.
    #[allow(clippy::cast_precision_loss)]
    let nf = n as f64;

    // Entropies. H(A) = -Σ p_a · ln(p_a). Same for B.
    let mut h_a = 0.0_f64;
    for &m in &marg_a {
        if m > 0 {
            #[allow(clippy::cast_precision_loss)]
            let p = m as f64 / nf;
            h_a -= p * p.ln();
        }
    }
    let mut h_b = 0.0_f64;
    for &m in &marg_b {
        if m > 0 {
            #[allow(clippy::cast_precision_loss)]
            let p = m as f64 / nf;
            h_b -= p * p.ln();
        }
    }

    // Degenerate-entropy short-circuits per the doc comment.
    if h_a == 0.0 && h_b == 0.0 {
        return 1.0;
    }
    if h_a == 0.0 || h_b == 0.0 {
        return 0.0;
    }

    // Mutual information.
    // I(A,B) = Σ_ij p_ij · ln( p_ij / (p_a · p_b) )
    //        = Σ_ij (c_ij / n) · ln( (c_ij · n) / (marg_a_i · marg_b_j) )
    // Skip terms where c_ij = 0 (0·ln 0 = 0).
    let mut mi = 0.0_f64;
    for (i, row) in contingency.iter().enumerate() {
        for (j, &c_ij) in row.iter().enumerate() {
            if c_ij == 0 {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let c = c_ij as f64;
            #[allow(clippy::cast_precision_loss)]
            let a_count = marg_a[i] as f64;
            #[allow(clippy::cast_precision_loss)]
            let b_count = marg_b[j] as f64;
            mi += (c / nf) * ((c * nf) / (a_count * b_count)).ln();
        }
    }

    // 2 * I / (H_a + H_b). Bounded to [0, 1] in exact arithmetic;
    // clamp to absorb any tiny float drift.
    let raw = 2.0 * mi / (h_a + h_b);
    raw.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::nmi;
    use crate::community::Partition;

    #[test]
    fn identical_partitions_score_one() {
        let a = Partition::new(vec![0, 0, 0, 1, 1, 1, 2, 2, 2]);
        let b = Partition::new(vec![0, 0, 0, 1, 1, 1, 2, 2, 2]);
        assert!((nmi(&a, &b) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn relabeled_partition_scores_one() {
        // NMI is label-permutation invariant. Same buckets, different labels.
        let a = Partition::new(vec![0, 0, 1, 1, 2, 2]);
        let b = Partition::new(vec![9, 9, 7, 7, 3, 3]);
        assert!((nmi(&a, &b) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn is_symmetric() {
        let a = Partition::new(vec![0, 0, 1, 1, 2, 0]);
        let b = Partition::new(vec![0, 1, 0, 1, 1, 1]);
        assert!((nmi(&a, &b) - nmi(&b, &a)).abs() < 1e-12);
    }

    #[test]
    fn both_trivial_partitions_score_one() {
        // Everyone in one community in both. They "agree" by convention.
        let a = Partition::new(vec![0; 10]);
        let b = Partition::new(vec![0; 10]);
        assert!((nmi(&a, &b) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn one_trivial_one_split_scores_zero() {
        // A says "one community"; B splits it. The trivial side has
        // zero entropy; nothing in common.
        let a = Partition::new(vec![0; 10]);
        let b = Partition::new(vec![0, 0, 0, 0, 0, 1, 1, 1, 1, 1]);
        assert!(nmi(&a, &b).abs() < 1e-12);
    }

    #[test]
    fn known_two_by_two_case() {
        // Hand-computable: 4 nodes, partition A puts {0,1} | {2,3},
        // partition B puts {0,2} | {1,3}. Marginals are uniform (2,2)
        // on both sides; contingency is the identity scaled, giving
        // I = 0, NMI = 0.
        let a = Partition::new(vec![0, 0, 1, 1]);
        let b = Partition::new(vec![0, 1, 0, 1]);
        let score = nmi(&a, &b);
        assert!(
            score.abs() < 1e-12,
            "expected NMI≈0 for orthogonal 2x2 split, got {score}"
        );
    }

    #[test]
    fn nested_partition_scores_between_zero_and_one() {
        // A is {0,1,2,3} | {4,5,6,7}.
        // B refines A: {0,1} | {2,3} | {4,5} | {6,7}.
        // Every B bucket is fully inside an A bucket — partial agreement.
        let a = Partition::new(vec![0, 0, 0, 0, 1, 1, 1, 1]);
        let b = Partition::new(vec![0, 0, 1, 1, 2, 2, 3, 3]);
        let score = nmi(&a, &b);
        assert!(
            score > 0.0 && score < 1.0,
            "expected 0 < NMI < 1 for refinement, got {score}"
        );
    }

    #[test]
    fn vacuous_partitions_score_one() {
        // Edge case the doc comment names explicitly.
        let a = Partition::new(Vec::<usize>::new());
        let b = Partition::new(Vec::<usize>::new());
        assert!((nmi(&a, &b) - 1.0).abs() < 1e-12);
    }

    #[test]
    #[should_panic(expected = "partitions cover different numbers of nodes")]
    fn mismatched_node_counts_panic() {
        let a = Partition::new(vec![0, 0, 1]);
        let b = Partition::new(vec![0, 0]);
        let _ = nmi(&a, &b);
    }
}
