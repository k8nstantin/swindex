//! Incremental-maintenance interface (Phase 1 scaffolding for issue #27).
//!
//! # What this module ships
//!
//! The **types** all future incremental-maintenance work will use, plus
//! a stub policy ([`NeverRebalance`]) that does nothing. Real
//! rebalancing — threshold-driven (Phase 2) and full Ada-IVF
//! (Phase 3+) — drops in behind the [`MaintenancePolicy`] trait
//! without touching the rest of the index.
//!
//! # Why a separate module
//!
//! `index.rs` is already large and owns the persistence-layer machinery.
//! Maintenance concerns — drift tracking, policy decisions, rebalancing —
//! are distinct enough that growing them inside `index.rs` would obscure
//! both. Splitting now means future Ada-IVF code has somewhere coherent
//! to live.
//!
//! # The contract
//!
//! 1. [`SwIndex::insert_node`](crate::index::SwIndex::insert_node)
//!    appends to existing structure and increments per-cluster drift
//!    counters. **It never re-clusters.**
//! 2. [`SwIndex::maintain`](crate::index::SwIndex::maintain) asks a
//!    [`MaintenancePolicy`] what to do with the current drift state
//!    and executes its decisions. With [`NeverRebalance`] it does
//!    nothing — drift accumulates indefinitely.
//! 3. Future policies (`ThresholdRebalance`, `AdaptiveAdaIvf`) will
//!    return [`MaintenanceAction`]s that today's enum doesn't have
//!    (`RebalanceCluster`, `RebuildAll`). Phase 1 ships the
//!    `DoNothing` variant only — adding new variants is the
//!    forward-compat path.
//!
//! # Non-goals for Phase 1
//!
//! * Edge inserts between existing nodes (need re-clustering, which is
//!   what Phase 2+ does).
//! * Deletes / tombstones.
//! * Hub re-detection on insert (new nodes default to non-hub).
//! * Region re-detection on insert (new nodes inherit their cluster's
//!   region, or the default region for a new singleton).

use std::collections::BTreeMap;

/// A decision a [`MaintenancePolicy`] hands back from
/// [`MaintenancePolicy::decide`]. Today there's exactly one variant —
/// the no-op. Phase 2 will add `RebalanceCluster(u32)`; Phase 3 will
/// add `RebuildAll`.
///
/// Variants are added behind a `#[non_exhaustive]` annotation so
/// callers must use a wildcard arm — this lets us add variants in
/// future versions without breaking downstream code that did its own
/// `match` on the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MaintenanceAction {
    /// The policy decided no rebalancing is needed right now.
    DoNothing,
}

/// What the index knows about insert pressure since the last
/// rebalance. Read-only snapshot; future Ada-IVF policies will read
/// this to decide whether to re-Leiden a cluster.
///
/// `delta_inserts` is the count of `insert_node` calls that assigned a
/// node to that cluster since the cluster's last rebuild
/// (`generation`). For a freshly built index every cluster has
/// `generation = 0` and `delta_inserts = 0`.
#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    /// Per-cluster `{generation, delta_inserts}`. `BTreeMap` for
    /// deterministic iteration order in tests and human-readable
    /// reports.
    pub per_cluster: BTreeMap<u32, ClusterDrift>,
}

/// Drift state for a single cluster. The atomic unit of per-cluster
/// maintenance bookkeeping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClusterDrift {
    /// Monotonic generation counter — increments on every rebuild
    /// (full or partial) of this cluster. v0.1.0 indexes have
    /// generation 0 across the board; the first rebuild after
    /// `insert_node` lands raises affected clusters to 1.
    pub generation: u64,
    /// Inserts assigned to this cluster since the last rebuild.
    pub delta_inserts: u32,
}

impl DriftReport {
    /// Total inserts across all clusters. Useful for "should we look
    /// at maintenance at all?" heuristics in custom policies.
    #[must_use]
    pub fn total_inserts(&self) -> u64 {
        self.per_cluster
            .values()
            .map(|d| u64::from(d.delta_inserts))
            .sum()
    }

    /// Number of clusters tracked in this report — typically the
    /// cluster count from the last build plus any singletons created
    /// by inserts with no known seed neighbors.
    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.per_cluster.len()
    }
}

/// Summary of what [`SwIndex::maintain`](crate::index::SwIndex::maintain)
/// actually did. With [`NeverRebalance`] this is always empty; future
/// policies will fill it with the actions they executed.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceReport {
    /// Actions the policy returned and the index applied, in the order
    /// they were applied. Empty `Vec` means "policy returned
    /// `DoNothing` for every cluster" — that's a valid result.
    pub actions_taken: Vec<MaintenanceAction>,
}

/// A policy for deciding what (if anything) to rebalance during
/// `SwIndex::maintain`. Implementors read the [`DriftReport`] and
/// return [`MaintenanceAction`]s.
///
/// Phase 1 ships the single implementation [`NeverRebalance`]. Phase 2
/// will add `ThresholdRebalance`; Phase 3 will add `AdaptiveAdaIvf`.
/// The trait is intentionally tiny — that's the whole point of having
/// it before there's only one consumer.
pub trait MaintenancePolicy {
    /// Inspect drift state and return the actions the index should
    /// take. The order of returned actions is the order they'll be
    /// applied; policies that depend on intra-action consistency
    /// should be careful with ordering.
    ///
    /// Returning an empty `Vec` is equivalent to "no maintenance
    /// needed right now."
    fn decide(&self, drift: &DriftReport) -> Vec<MaintenanceAction>;
}

/// The default Phase 1 policy: do nothing, ever. Drift accumulates
/// indefinitely; clusters get progressively less accurate over time;
/// the caller is expected to periodically `build_from_source` from
/// scratch when accuracy matters.
///
/// This is the *correct* default for Phase 1 — without real Ada-IVF
/// logic, any "rebalance" we attempted would either be a no-op or
/// strictly worse than the static partition. Users who want quality
/// preservation under inserts will swap in `ThresholdRebalance`
/// (Phase 2) or `AdaptiveAdaIvf` (Phase 3) when those land.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverRebalance;

impl MaintenancePolicy for NeverRebalance {
    fn decide(&self, _drift: &DriftReport) -> Vec<MaintenanceAction> {
        // The whole policy: do nothing. No allocations on the hot path.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterDrift, DriftReport, MaintenanceAction, MaintenancePolicy, NeverRebalance};

    #[test]
    fn never_rebalance_always_returns_empty_actions() {
        let policy = NeverRebalance;

        // Empty drift -> empty actions.
        let mut drift = DriftReport::default();
        assert!(policy.decide(&drift).is_empty());

        // Drift with content -> still empty.
        drift.per_cluster.insert(
            0,
            ClusterDrift {
                generation: 0,
                delta_inserts: 1000,
            },
        );
        drift.per_cluster.insert(
            7,
            ClusterDrift {
                generation: 3,
                delta_inserts: 42,
            },
        );
        assert!(policy.decide(&drift).is_empty());
    }

    #[test]
    fn drift_report_summaries() {
        let mut drift = DriftReport::default();
        drift.per_cluster.insert(
            0,
            ClusterDrift {
                generation: 0,
                delta_inserts: 10,
            },
        );
        drift.per_cluster.insert(
            1,
            ClusterDrift {
                generation: 0,
                delta_inserts: 25,
            },
        );
        drift.per_cluster.insert(
            2,
            ClusterDrift {
                generation: 1,
                delta_inserts: 0,
            },
        );

        assert_eq!(drift.cluster_count(), 3);
        assert_eq!(drift.total_inserts(), 35);
    }

    #[test]
    fn maintenance_action_variants_compile() {
        // Smoke: the variants exist and are constructible. If a future
        // PR adds variants, this catches obviously broken matches.
        let _ = MaintenanceAction::DoNothing;
    }
}
