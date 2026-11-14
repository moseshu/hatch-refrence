//! Task types: one row of the unified ledger (P1 D6) and its states.
//!
//! A task is any unit of work the agent owns — a worker job, a sub-agent
//! run, a scheduled job run, a one-off task. The ledger is the single
//! place a process restart looks to find unfinished work; each subsystem
//! keeps no second book of its own.
//!
//! `TaskStatus::Blocked` is a *task* state (this task waits on something
//! external). It is a different object from `PlanStatus::Blocked` (a
//! *plan* state, D1) — same word, different ledger, do not confuse them.

use serde::{Deserialize, Serialize};

/// Default execution-lease TTL in seconds (M0): 5 minutes. The watchdog
/// (D7) treats a `running` task as stale after 300s, so a lease that
/// outlives several missed heartbeats (beat interval ~60s) never trips
/// it, while a dead owner is still reclaimed within ~5 minutes.
pub const DEFAULT_LEASE_TTL_SECS: u64 = 300;

/// The execution lease (M0): the one credential that lets a worker
/// write a running task. `start`/`retry`/`unblock`/`reclaim` issue a
/// fresh lease (generation + 1); `heartbeat` proves the owner still
/// holds it; every guarded write (step/event/unit records, and the
/// running-state transitions) is an UPDATE conditioned on owner +
/// generation, so a stale owner matches zero rows instead of
/// overwriting a successor's work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner: String,
    pub generation: u64,
}

/// Why a task is `Blocked` (M0): the structured reason the runtime
/// (and the user-facing task list) shows, instead of parsing free text
/// out of `error`. `None` is stored as ''.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    /// Not blocked (stored as '').
    None,
    /// Waits on a human approval.
    Approval,
    /// Waits on a login / credential the user must supply.
    Login,
    /// Waits on information only the user can give.
    MissingInfo,
    /// Waits on a result that could not be verified yet.
    Verification,
    /// Paused by the user (task 5): the worker stops at the next step
    /// boundary and the task stays put until resumed. Unlike the other
    /// kinds it is not something the task waits *on* — it is a user
    /// hold, and `task run --task` clears it via `unblock`.
    Paused,
    /// Waits on child tasks (task 7): the parent delegated one or
    /// more branches and cannot advance until they report. A child
    /// completion unblocks it; reconcile never moves it.
    Children,
}

impl BlockedKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockedKind::None => "",
            BlockedKind::Approval => "approval",
            BlockedKind::Login => "login",
            BlockedKind::MissingInfo => "missing_info",
            BlockedKind::Verification => "verification",
            BlockedKind::Paused => "paused",
            BlockedKind::Children => "children",
        }
    }

    pub fn parse(s: &str) -> BlockedKind {
        match s {
            "approval" => BlockedKind::Approval,
            "login" => BlockedKind::Login,
            "missing_info" => BlockedKind::MissingInfo,
            "verification" => BlockedKind::Verification,
            "paused" => BlockedKind::Paused,
            "children" => BlockedKind::Children,
            _ => BlockedKind::None,
        }
    }
}

/// One task's lifecycle. Terminal: `Done`, `Failed`, `Cancelled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created, never started.
    Queued,
    /// An owner is working on it right now.
    Running,
    /// Paused for a retry: the system will pick it up again.
    Waiting,
    /// Paused on something external (an approval, the user, a dependency).
    /// Reconcile never moves it — only the thing it waits on does.
    Blocked,
    /// Finished successfully.
    Done,
    /// Finished unsuccessfully (gave up, or retries exhausted).
    Failed,
    /// Cancelled by the user (control plane, §7.2). Terminal. A
    /// cancelled task is never retried or reconciled; a worker that
    /// notices it stops claiming new steps and lets in-flight calls
    /// reconcile against the ledger.
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> TaskStatus {
        match s {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "waiting" => TaskStatus::Waiting,
            "blocked" => TaskStatus::Blocked,
            "done" => TaskStatus::Done,
            "cancelled" => TaskStatus::Cancelled,
            _ => TaskStatus::Failed,
        }
    }

    /// Terminal: no further transition is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled)
    }

    /// Whether `from` may move to `to`. The store enforces one row of
    /// this table per method — `start` / `retry` / `unblock` all land in
    /// `running`, so each names its own source state instead of sharing
    /// this check. Legal moves:
    /// * queued → running (start), queued → failed (refuse before start),
    ///   queued → cancelled (user cancel before start), queued → blocked
    ///   (user pause — the only queued → blocked move);
    /// * running → waiting (pause for retry), running → blocked (external
    ///   dependency, or user pause), running → done, running → failed, running →
    ///   cancelled (user cancel mid-run);
    /// * waiting → running (retry, bumps `retries`), waiting → failed,
    ///   waiting → cancelled, waiting → blocked (user pause);
    /// * blocked → running (the thing it waited on arrived), blocked →
    ///   failed, blocked → cancelled.
    pub fn can_transition_to(from: TaskStatus, to: TaskStatus) -> bool {
        matches!(
            (from, to),
            (TaskStatus::Queued, TaskStatus::Running)
                | (TaskStatus::Queued, TaskStatus::Failed)
                | (TaskStatus::Queued, TaskStatus::Cancelled)
                | (TaskStatus::Queued, TaskStatus::Blocked)
                | (TaskStatus::Running, TaskStatus::Waiting)
                | (TaskStatus::Running, TaskStatus::Blocked)
                | (TaskStatus::Running, TaskStatus::Done)
                | (TaskStatus::Running, TaskStatus::Failed)
                | (TaskStatus::Running, TaskStatus::Cancelled)
                | (TaskStatus::Waiting, TaskStatus::Running)
                | (TaskStatus::Waiting, TaskStatus::Failed)
                | (TaskStatus::Waiting, TaskStatus::Cancelled)
                | (TaskStatus::Waiting, TaskStatus::Blocked)
                | (TaskStatus::Blocked, TaskStatus::Running)
                | (TaskStatus::Blocked, TaskStatus::Failed)
                | (TaskStatus::Blocked, TaskStatus::Cancelled)
        )
    }
}

/// One row of `tasks`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// What kind of work this is (`worker`, `subagent`, `scheduler`, ...).
    /// Free-form, like `jobs.kind`.
    pub kind: String,
    /// Human-readable title.
    pub title: String,
    /// Who owns it (`session:<id>`, `daemon`, `worker:<name>`, ...).
    pub owner: String,
    /// Parent task id ('' = no parent).
    pub parent_task: String,
    pub status: TaskStatus,
    /// Restarts after waiting (bumped by `retry`, not by `start`).
    pub retries: u32,
    /// Retry bound (default 3, the same bound D8 uses for transient
    /// failures). Reconcile fails a stale `running` task once this is hit.
    pub max_retries: u32,
    /// Last proof the owner is alive; set at creation, refreshed by
    /// `heartbeat()` while the task runs. D6 only stores the field —
    /// live heartbeat updates and liveness probing are D7's job.
    pub last_heartbeat: String,
    /// Task-level idempotency key (None = no dedup). Re-submitting the
    /// same key returns the existing row instead of creating a new one.
    pub idempotency_key: Option<String>,
    /// Last error / note (why it waits, failed, or was revived).
    pub error: String,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    /// Execution lease (M0): the owner currently allowed to write this
    /// task, '' when no lease is held. See [`Lease`].
    pub lease_owner: String,
    /// Lease generation, +1 on every claim (`start`/`retry`/`unblock`/
    /// `reclaim`). A write with an older generation is stale and must
    /// fail — the UPDATE matches zero rows.
    pub lease_generation: u64,
    /// TTL (seconds) the current lease was issued with; `heartbeat`
    /// refreshes `lease_expires_at` by this amount.
    pub lease_ttl_secs: u64,
    /// RFC3339 UTC instant the lease expires; '' when no lease is held.
    /// `reclaim` only works after this instant (and only for `running`).
    pub lease_expires_at: String,
    /// Structured reason this task is `Blocked` ('' = not blocked).
    /// Stored as a string so old rows read back as [`BlockedKind::None`].
    pub blocked_kind: BlockedKind,
    /// The hatch-plan plan this task runs ('' = none: the
    /// meeting-brief kind keeps its steps in `task_steps`).
    #[serde(default)]
    pub plan_id: String,
    /// §10.2 plan revision (M1): commands carry the version they
    /// expect; approvals are bound to the version they were requested
    /// under. A bump invalidates the old version's pending approvals.
    #[serde(default = "default_plan_version")]
    pub plan_version: u64,
    /// Model-call budget (M1, task 3): 0 = unlimited. When the worker's
    /// cumulative model calls reach it, the task pauses (waiting) with
    /// its completed steps still readable — it never loses work.
    #[serde(default)]
    pub max_model_calls: u64,
    /// Mail source (M2, schema v15): `fixture` for an eval world,
    /// `gmail` for a real-account run; '' for pre-v15 tasks (fixture).
    /// An approval bound to this task executes with this source's
    /// sender.
    #[serde(default)]
    pub source: String,
}

fn default_plan_version() -> u64 {
    1
}

impl Task {
    /// The lease this task currently carries, if any.
    pub fn lease(&self) -> Option<Lease> {
        if self.lease_owner.is_empty() {
            None
        } else {
            Some(Lease { owner: self.lease_owner.clone(), generation: self.lease_generation })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip_and_terminal() {
        for s in [
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::Waiting,
            TaskStatus::Blocked,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(TaskStatus::parse(s.as_str()), s);
        }
        assert!(TaskStatus::Done.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(!TaskStatus::Blocked.is_terminal());
        assert_eq!(TaskStatus::parse("bogus"), TaskStatus::Failed);
    }

    #[test]
    fn blocked_kind_roundtrip() {
        for k in [
            BlockedKind::None,
            BlockedKind::Approval,
            BlockedKind::Login,
            BlockedKind::MissingInfo,
            BlockedKind::Verification,
            BlockedKind::Paused,
        ] {
            assert_eq!(BlockedKind::parse(k.as_str()), k);
        }
        assert_eq!(BlockedKind::parse(""), BlockedKind::None);
        assert_eq!(BlockedKind::parse("bogus"), BlockedKind::None);
    }

    #[test]
    fn transition_table() {
        assert!(TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Running));
        assert!(TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Failed));
        assert!(TaskStatus::can_transition_to(TaskStatus::Running, TaskStatus::Waiting));
        assert!(TaskStatus::can_transition_to(TaskStatus::Running, TaskStatus::Blocked));
        assert!(TaskStatus::can_transition_to(TaskStatus::Running, TaskStatus::Done));
        assert!(TaskStatus::can_transition_to(TaskStatus::Waiting, TaskStatus::Running));
        assert!(TaskStatus::can_transition_to(TaskStatus::Blocked, TaskStatus::Running));
        assert!(TaskStatus::can_transition_to(TaskStatus::Blocked, TaskStatus::Failed));
        // Cancel is legal from any non-terminal state, terminal from none.
        assert!(TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Cancelled));
        assert!(TaskStatus::can_transition_to(TaskStatus::Running, TaskStatus::Cancelled));
        assert!(TaskStatus::can_transition_to(TaskStatus::Waiting, TaskStatus::Cancelled));
        assert!(TaskStatus::can_transition_to(TaskStatus::Blocked, TaskStatus::Cancelled));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Done, TaskStatus::Cancelled));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Cancelled, TaskStatus::Running));
        // Pause (task 5) is the only queued/waiting → blocked move.
        assert!(TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Blocked));
        assert!(TaskStatus::can_transition_to(TaskStatus::Waiting, TaskStatus::Blocked));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Done, TaskStatus::Blocked));
        // Illegal: anything out of a terminal state, and skips.
        assert!(!TaskStatus::can_transition_to(TaskStatus::Done, TaskStatus::Running));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Failed, TaskStatus::Running));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Done));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Waiting));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Waiting, TaskStatus::Done));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Blocked, TaskStatus::Waiting));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Running, TaskStatus::Queued));
    }
}
