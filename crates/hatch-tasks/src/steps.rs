//! Plan steps (M0): the durable step record of a task.
//!
//! One `task_steps` row per plan step. A step is `pending` → `running`
//! → `done` / `failed`; `attempts` counts how often it was taken up
//! (a resume counts as an attempt). Step completion and its event
//! commit in one transaction (see `complete_step`), so a crash can
//! never leave a step done without its event, or an event without its
//! step — the recovery drill (item 7) asserts exactly that.

use crate::events::TaskEvent;
use crate::task::Lease;
use crate::unit::{ExecStatus, UnitStore};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, DbResultExt, OptionalExtension};
use serde::{Deserialize, Serialize};

/// A plan step the worker wants to run (used by `ensure_steps`).
#[derive(Clone, Debug)]
pub struct StepSpec {
    pub step_id: String,
    pub title: String,
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// One row of `task_steps`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskStep {
    pub task_id: String,
    pub idx: i64,
    pub step_id: String,
    pub title: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub status: ExecStatus,
    pub attempts: u32,
    pub input_refs: Vec<String>,
    pub approval_id: String,
    pub receipt_id: String,
    pub output: String,
    pub created_at: String,
    pub updated_at: String,
}

const STEP_COLS: &str = "task_id, idx, step_id, title, tool, arguments, status, attempts, input_refs, approval_id, receipt_id, output, created_at, updated_at";

fn step_row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<TaskStep> {
    Ok(TaskStep {
        task_id: r.get(0)?,
        idx: r.get(1)?,
        step_id: r.get(2)?,
        title: r.get(3)?,
        tool: r.get(4)?,
        arguments: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null),
        status: ExecStatus::parse(&r.get::<_, String>(6)?),
        attempts: r.get::<_, i64>(7)? as u32,
        input_refs: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
        approval_id: r.get(9)?,
        receipt_id: r.get(10)?,
        output: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

impl<'a> UnitStore<'a> {
    /// Insert the plan's steps if they are not there yet (idempotent on
    /// `(task_id, idx)`), and return the stored steps in order.
    pub fn ensure_steps(
        &self,
        task_id: &str,
        lease: &Lease,
        steps: &[StepSpec],
        now: &Timestamp,
    ) -> Result<Vec<TaskStep>> {
        let ts = to_rfc3339(now);
        self.db().transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            for (idx, s) in steps.iter().enumerate() {
                let args = serde_json::to_string(&s.arguments).unwrap_or_else(|_| "{}".into());
                c.execute(
                    "INSERT OR IGNORE INTO task_steps(task_id, idx, step_id, title, tool, arguments, status, attempts, input_refs, approval_id, receipt_id, output, created_at, updated_at) VALUES(?1,?2,?3,?4,?5,?6,'pending',0,'[]','','','',?7,?7)",
                    params![task_id, idx as i64, s.step_id, s.title, s.tool, args, ts],
                )
                .db()?;
            }
            Ok(())
        })?;
        self.list_steps(task_id)
    }

    pub fn list_steps(&self, task_id: &str) -> Result<Vec<TaskStep>> {
        let conn = self.db().conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {STEP_COLS} FROM task_steps WHERE task_id = ?1 ORDER BY idx"
            ))
            .db()?;
        let rows = st.query_map(params![task_id], step_row).db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    pub fn get_step(&self, task_id: &str, step_id: &str) -> Result<Option<TaskStep>> {
        let conn = self.db().conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {STEP_COLS} FROM task_steps WHERE task_id = ?1 AND step_id = ?2"
            ))
            .db()?;
        st.query_row(params![task_id, step_id], step_row)
            .optional()
            .db()
    }

    /// Record the approval a step is waiting on (M1): the worker stores
    /// the approval id the moment dispatch queues it, so a resume finds
    /// the same approval instead of dispatching a duplicate.
    pub fn set_step_approval(
        &self,
        task_id: &str,
        lease: &Lease,
        step_id: &str,
        approval_id: &str,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        self.db().transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            c.execute(
                "UPDATE task_steps SET approval_id = ?3, updated_at = ?4 WHERE task_id = ?1 AND step_id = ?2",
                params![task_id, step_id, approval_id, ts],
            )
            .db()?;
            Ok(())
        })
    }

    /// Record the receipt of a step's side effect (M1): the ledger key
    /// of the executed action, so `task show` can point at exactly what
    /// happened and a resume never re-executes it.
    pub fn set_step_receipt(
        &self,
        task_id: &str,
        lease: &Lease,
        step_id: &str,
        receipt_id: &str,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        self.db().transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            c.execute(
                "UPDATE task_steps SET receipt_id = ?3, updated_at = ?4 WHERE task_id = ?1 AND step_id = ?2",
                params![task_id, step_id, receipt_id, ts],
            )
            .db()?;
            Ok(())
        })
    }

    /// Mark a step `running` and count the attempt (a resume counts as
    /// an attempt too — it proves the step was taken up again).
    pub fn start_step(
        &self,
        task_id: &str,
        lease: &Lease,
        step_id: &str,
        now: &Timestamp,
    ) -> Result<TaskStep> {
        let ts = to_rfc3339(now);
        self.db().transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            c.execute(
                "UPDATE task_steps SET status = 'running', attempts = attempts + 1, updated_at = ?3 WHERE task_id = ?1 AND step_id = ?2 AND status IN ('pending','running')",
                params![task_id, step_id, ts],
            )
            .db()?;
            Ok(())
        })?;
        self.get_step(task_id, step_id)?
            .ok_or_else(|| Error::NotFound(format!("step {step_id} 不存在")))
    }

    /// Complete a step AND append its event in one transaction: the two
    /// can never split across a crash, so recovery never sees a done
    /// step without its event, and never re-runs one that is done.
    pub fn complete_step(
        &self,
        task_id: &str,
        lease: &Lease,
        step_id: &str,
        status: ExecStatus,
        output: &str,
        now: &Timestamp,
    ) -> Result<(TaskStep, TaskEvent)> {
        let ts = to_rfc3339(now);
        let kind = match status {
            ExecStatus::Done => "step_done",
            ExecStatus::Failed => "step_failed",
            _ => "step_done",
        };
        let event = self.db().transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            c.execute(
                "UPDATE task_steps SET status = ?3, output = ?4, updated_at = ?5 WHERE task_id = ?1 AND step_id = ?2",
                params![task_id, step_id, status.as_str(), output, ts],
            )
            .db()?;
            crate::events::insert_event(c, task_id, kind, step_id, output, &ts)
        })?;
        let step = self
            .get_step(task_id, step_id)?
            .ok_or_else(|| Error::NotFound(format!("step {step_id} 不存在")))?;
        Ok((step, event))
    }
}
