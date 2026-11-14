//! `hatch-tasks` — P1 D6, the unified task ledger.
//!
//! One durable row per task of any kind (worker job, sub-agent run,
//! scheduled job run, one-off task), with a six-state machine
//! (`queued / running / waiting / blocked / done / failed`), owner,
//! parent task, retry count, last heartbeat, and a task-level
//! idempotency key. A process restart reconciles the ledger: tasks a
//! previous process left in `running` whose heartbeat is stale are put
//! back to `waiting` (retries left) or `failed` (retries exhausted),
//! so no task stays stuck in `running` forever.
//!
//! * [`task`]  — [`Task`] / [`TaskStatus`] and the transition table.
//! * [`store`] — [`TaskStore`], the SQLite persistence, transitions,
//!   heartbeat, and startup reconcile ([`TaskStore::reconcile`]).
//! * [`unit`] — [`UnitStore`], the durable execution unit (M0): plan
//!   steps, model decision batches, per-call results, and the
//!   append-only event log a worker resumes from after a crash.
//! * [`events`] — [`EventLog`], the append-only per-task event log
//!   (M0): sequential, cursorable, lease-guarded.
//! * [`recovery`] — the resume side of the unit record (M0):
//!   [`ResumePosition`], pending calls, unit results.
//! * [`watchdog`] — [`WatchdogConfig`] and [`probe_once`]: the live
//!   liveness probe (D7) over the same reconcile.
//!
//! Deliberately not here: snapshots (D11), pause/resume (D5). And no
//! producer writes task rows yet — worker / scheduler / sub-agent
//! adoption is a follow-up; this crate is the ledger they will share.

pub mod context;
pub mod delegation;
pub mod events;
pub mod recovery;
pub mod steps;
pub mod store;
pub mod task;
pub mod task_approvals;
pub mod unit;
pub mod watchdog;

pub use context::{EvidenceRef, TaskContext, TaskContextStore, TaskContextView};
pub use delegation::{
    all_consumed, has_pending_children, ChildResult, ChildSpec, ConsumeOutcome, Delegation,
    DelegationStatus, DelegationStore, FailureReport, FileWrite, MergeConflict,
};
pub use events::{EventLog, TaskEvent};
pub use recovery::ResumePosition;
pub use steps::{StepEdit, StepSpec, TaskStep};
pub use store::{ReconcileReport, TaskStore, DEFAULT_STALE_AFTER_SECS};
pub use task::{BlockedKind, Lease, Task, TaskStatus, DEFAULT_LEASE_TTL_SECS};
pub use task_approvals::{TaskApproval, TaskApprovals, TaskCommand, TaskCommands};
pub use unit::{ExecStatus, ExecUnit, UnitCall, UnitCallSpec, UnitStore};
pub use watchdog::{probe_once, WatchdogConfig};
