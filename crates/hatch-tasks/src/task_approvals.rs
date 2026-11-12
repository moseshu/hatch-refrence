//! Approvals bound to a task's plan version (M1, §10.2).
//!
//! When the worker asks for an approval, it records which plan version
//! that approval belongs to. A later plan version (committed by a user
//! command) invalidates the old version's approvals — eagerly, by
//! marking them, and authoritatively, by the version check at approve
//! time, which reads this table. The link lives here, not in the
//! approval payload: the payload is the exact action the user reviewed,
//! and task bookkeeping must never rewrite it.

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

/// One link: this approval was requested for this task while its plan
/// was at this version.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskApproval {
    pub task_id: String,
    pub approval_id: String,
    pub plan_version: u64,
    pub step_id: String,
    pub created_at: String,
}

pub struct TaskApprovals<'a> {
    db: &'a Db,
}

impl<'a> TaskApprovals<'a> {
    pub fn new(db: &'a Db) -> Self {
        TaskApprovals { db }
    }

    /// Record the link (idempotent on the pair — re-linking the same
    /// approval for the same task is a no-op, so a resumed worker that
    /// re-dispatches never duplicates it).
    pub fn link(
        &self,
        task_id: &str,
        approval_id: &str,
        plan_version: u64,
        step_id: &str,
        now: &Timestamp,
    ) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT OR IGNORE INTO task_approvals(task_id, approval_id, plan_version, step_id, created_at) VALUES(?1,?2,?3,?4,?5)",
                params![task_id, approval_id, plan_version as i64, step_id, to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }

    /// The task + version an approval was requested under, if it is a
    /// task approval at all (plan approvals and triage approvals have
    /// no row here and are unaffected by task versions).
    pub fn linked_version(&self, approval_id: &str) -> Result<Option<(String, u64)>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT task_id, plan_version FROM task_approvals WHERE approval_id = ?1 ORDER BY created_at LIMIT 1",
            )
            .db()?;
        st.query_row(params![approval_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })
        .optional()
        .db()
    }

    /// All approvals linked to a task, oldest first.
    pub fn for_task(&self, task_id: &str) -> Result<Vec<TaskApproval>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT task_id, approval_id, plan_version, step_id, created_at FROM task_approvals WHERE task_id = ?1 ORDER BY created_at",
            )
            .db()?;
        let rows = st
            .query_map(params![task_id], |r| {
                Ok(TaskApproval {
                    task_id: r.get(0)?,
                    approval_id: r.get(1)?,
                    plan_version: r.get::<_, i64>(2)? as u64,
                    step_id: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// Approval ids linked to a task at a version older than `current`
    /// — the set a version bump invalidates.
    pub fn older_than(&self, task_id: &str, current: u64) -> Result<Vec<String>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT approval_id FROM task_approvals WHERE task_id = ?1 AND plan_version < ?2",
            )
            .db()?;
        let rows = st
            .query_map(params![task_id, current as i64], |r| r.get(0))
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }
}

/// One §10.2 command, durably recorded (M1).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskCommand {
    pub id: String,
    pub task_id: String,
    pub expected_version: u64,
    pub new_version: u64,
    pub user_id: String,
    pub summary: String,
    pub status: String,
    pub created_at: String,
}

pub struct TaskCommands<'a> {
    db: &'a Db,
}

impl<'a> TaskCommands<'a> {
    pub fn new(db: &'a Db) -> Self {
        TaskCommands { db }
    }

    /// Record a command. The caller already validated the expected
    /// version and bumped the plan; this only persists the record.
    pub fn record(
        &self,
        task_id: &str,
        expected_version: u64,
        new_version: u64,
        user_id: &str,
        summary: &str,
        status: &str,
        now: &Timestamp,
    ) -> Result<TaskCommand> {
        let id = hatch_core::ids::random_id("tcmd");
        self.db
            .conn()
            .execute(
                "INSERT INTO task_commands(id, task_id, expected_version, new_version, user_id, summary, status, created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    id,
                    task_id,
                    expected_version as i64,
                    new_version as i64,
                    user_id,
                    summary,
                    status,
                    to_rfc3339(now)
                ],
            )
            .db()?;
        Ok(TaskCommand {
            id,
            task_id: task_id.to_string(),
            expected_version,
            new_version,
            user_id: user_id.to_string(),
            summary: summary.to_string(),
            status: status.to_string(),
            created_at: to_rfc3339(now),
        })
    }

    /// Commands for a task, oldest first.
    pub fn for_task(&self, task_id: &str) -> Result<Vec<TaskCommand>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT id, task_id, expected_version, new_version, user_id, summary, status, created_at FROM task_commands WHERE task_id = ?1 ORDER BY created_at",
            )
            .db()?;
        let rows = st
            .query_map(params![task_id], |r| {
                Ok(TaskCommand {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    expected_version: r.get::<_, i64>(2)? as u64,
                    new_version: r.get::<_, i64>(3)? as u64,
                    user_id: r.get(4)?,
                    summary: r.get(5)?,
                    status: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }
}
