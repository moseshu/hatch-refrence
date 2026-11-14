//! Durable delegation (task 7): sub-tasks as first-class ledger tasks.
//!
//! A delegation binds one parent step to one child task. The child is
//! a normal row in `tasks` (parent_task set, its own budget and
//! checkpoints); the `task_delegations` row carries the binding, the
//! child's structured result, and the consume state. The parent
//! consumes a child at most once: `task_consumptions` is the marker,
//! and the whole consume — merge, marker, parent event, step
//! completion — commits in one transaction, so a crash before the
//! commit leaves nothing half-applied and a replay after the crash
//! finds the marker and does nothing.
//!
//! Merging never silently overwrites: a child write applies only
//! when the version it read is still the version the parent knows
//! (`task_resource_versions`); anything else is a conflict, recorded
//! on the parent, and the conflicting write is left out.

mod consume;
mod recovery;
mod store;
mod types;

pub use store::DelegationStore;
pub use types::{
    ChildResult, ChildSpec, ConsumeOutcome, Delegation, DelegationStatus, FailureReport, FileWrite,
    MergeConflict,
};

/// Whether a parent still has children that have not reported.
pub fn has_pending_children(delegations: &[Delegation]) -> bool {
    delegations.iter().any(|d| {
        matches!(
            d.status,
            DelegationStatus::Delegated | DelegationStatus::Running
        )
    })
}

/// Whether every delegation is consumed (nothing left to wait for).
pub fn all_consumed(delegations: &[Delegation]) -> bool {
    !delegations.is_empty()
        && delegations
            .iter()
            .all(|d| d.status == DelegationStatus::Consumed)
}
