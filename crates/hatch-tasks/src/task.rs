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

/// One task's lifecycle. Terminal: `Done`, `Failed`.
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
        }
    }

    pub fn parse(s: &str) -> TaskStatus {
        match s {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "waiting" => TaskStatus::Waiting,
            "blocked" => TaskStatus::Blocked,
            "done" => TaskStatus::Done,
            _ => TaskStatus::Failed,
        }
    }

    /// Terminal: no further transition is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Failed)
    }

    /// Whether `from` may move to `to`. The store enforces one row of
    /// this table per method — `start` / `retry` / `unblock` all land in
    /// `running`, so each names its own source state instead of sharing
    /// this check. Legal moves:
    /// * queued → running (start), queued → failed (refuse before start);
    /// * running → waiting (pause for retry), running → blocked (external
    ///   dependency), running → done, running → failed;
    /// * waiting → running (retry, bumps `retries`), waiting → failed;
    /// * blocked → running (the thing it waited on arrived), blocked → failed.
    pub fn can_transition_to(from: TaskStatus, to: TaskStatus) -> bool {
        matches!(
            (from, to),
            (TaskStatus::Queued, TaskStatus::Running)
                | (TaskStatus::Queued, TaskStatus::Failed)
                | (TaskStatus::Running, TaskStatus::Waiting)
                | (TaskStatus::Running, TaskStatus::Blocked)
                | (TaskStatus::Running, TaskStatus::Done)
                | (TaskStatus::Running, TaskStatus::Failed)
                | (TaskStatus::Waiting, TaskStatus::Running)
                | (TaskStatus::Waiting, TaskStatus::Failed)
                | (TaskStatus::Blocked, TaskStatus::Running)
                | (TaskStatus::Blocked, TaskStatus::Failed)
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
        ] {
            assert_eq!(TaskStatus::parse(s.as_str()), s);
        }
        assert!(TaskStatus::Done.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(!TaskStatus::Blocked.is_terminal());
        assert_eq!(TaskStatus::parse("bogus"), TaskStatus::Failed);
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
        // Illegal: anything out of a terminal state, and skips.
        assert!(!TaskStatus::can_transition_to(TaskStatus::Done, TaskStatus::Running));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Failed, TaskStatus::Running));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Done));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Queued, TaskStatus::Waiting));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Waiting, TaskStatus::Done));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Waiting, TaskStatus::Blocked));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Blocked, TaskStatus::Waiting));
        assert!(!TaskStatus::can_transition_to(TaskStatus::Running, TaskStatus::Queued));
    }
}
