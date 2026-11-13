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
    /// What the step must produce (task 5): the plan's `expected`,
    /// mirrored here so the planning list shows the acceptance without
    /// joining the plan. '' = no acceptance recorded.
    pub expected: String,
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
    pub expected: String,
    pub status: ExecStatus,
    pub attempts: u32,
    pub input_refs: Vec<String>,
    pub approval_id: String,
    pub receipt_id: String,
    pub output: String,
    /// How the step failed (task 6), mirrored from the plan's
    /// failure kind: '' = not failed, 'transient' / 'permanent' /
    /// 'needs_human'.
    pub failure_kind: String,
    pub created_at: String,
    pub updated_at: String,
}

const STEP_COLS: &str = "task_id, idx, step_id, title, tool, arguments, expected, status, attempts, input_refs, approval_id, receipt_id, output, failure_kind, created_at, updated_at";

fn step_row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<TaskStep> {
    Ok(TaskStep {
        task_id: r.get(0)?,
        idx: r.get(1)?,
        step_id: r.get(2)?,
        title: r.get(3)?,
        tool: r.get(4)?,
        arguments: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null),
        expected: r.get(6)?,
        status: ExecStatus::parse(&r.get::<_, String>(7)?),
        attempts: r.get::<_, i64>(8)? as u32,
        input_refs: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        approval_id: r.get(10)?,
        receipt_id: r.get(11)?,
        output: r.get(12)?,
        failure_kind: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

/// The editable fields of a pending step (task 5). `None` = leave the
/// field as it is; the caller requires at least one `Some`.
#[derive(Clone, Debug, Default)]
pub struct StepEdit {
    pub title: Option<String>,
    pub tool: Option<String>,
    pub arguments: Option<serde_json::Value>,
    pub expected: Option<String>,
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
                    "INSERT OR IGNORE INTO task_steps(task_id, idx, step_id, title, tool, arguments, expected, status, attempts, input_refs, approval_id, receipt_id, output, created_at, updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending',0,'[]','','','',?8,?8)",
                    params![task_id, idx as i64, s.step_id, s.title, s.tool, args, s.expected, ts],
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

    /// Record how a step failed (task 6), mirrored from the plan's
    /// failure kind. The worker reads this back to route the task:
    /// transient → wait and retry, needs-human → block, permanent →
    /// fail.
    pub fn set_step_failure_kind(
        &self,
        task_id: &str,
        lease: &Lease,
        step_id: &str,
        kind: &str,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        self.db().transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            c.execute(
                "UPDATE task_steps SET failure_kind = ?3, updated_at = ?4 WHERE task_id = ?1 AND step_id = ?2",
                params![task_id, step_id, kind, ts],
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

    /// Reset a step for a local replan (task 6): the plan row was
    /// reset to pending, so the task mirror must agree — otherwise
    /// `task show` would keep showing the step as done. The old
    /// receipt stays on the row (audit: what happened before the
    /// replan); the rerun overwrites it with the new receipt. Output,
    /// approval link and failure kind are cleared. No lease is
    /// needed: this is a control-plane write, like `cancel`. When the
    /// task never ran, there is no row yet and this is a no-op.
    pub fn reset_step_for_replan(
        &self,
        task_id: &str,
        step_id: &str,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        self.db()
            .conn()
            .execute(
                "UPDATE task_steps SET status = 'pending', output = '', approval_id = '', failure_kind = '', updated_at = ?3 WHERE task_id = ?1 AND step_id = ?2",
                params![task_id, step_id, ts],
            )
            .db()?;
        Ok(())
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

    /// Edit a pending step's content (task 5). The UPDATE is
    /// conditional on the step still being `pending` — or `running`
    /// but gated on an approval (`approval_id` set): a gated step's
    /// side effect has not executed yet, so it is still editable, and
    /// the version bump invalidates the old approval. Editing a
    /// running step with no approval link, or a done/failed step, is
    /// refused, so a worker's in-flight step can never be rewritten
    /// under it. The approval link is cleared — an approval requested
    /// for the old content is invalidated by the version bump and
    /// must be re-requested for the new content. No lease is
    /// needed: this is a control-plane write, like `cancel`.
    pub fn update_step(
        &self,
        task_id: &str,
        step_id: &str,
        edit: &StepEdit,
        now: &Timestamp,
    ) -> Result<TaskStep> {
        // The SET clause is built from fixed column names only, never
        // user input (same convention as `TaskStore::transition`).
        let mut sets: Vec<&str> = Vec::new();
        let mut values: Vec<String> = Vec::new();
        if let Some(v) = &edit.title {
            sets.push("title = ?");
            values.push(v.clone());
        }
        if let Some(v) = &edit.tool {
            sets.push("tool = ?");
            values.push(v.clone());
        }
        if let Some(v) = &edit.arguments {
            sets.push("arguments = ?");
            values.push(serde_json::to_string(v).unwrap_or_else(|_| "{}".into()));
        }
        if let Some(v) = &edit.expected {
            sets.push("expected = ?");
            values.push(v.clone());
        }
        if sets.is_empty() {
            return Err(Error::Invalid("没有要改的内容".into()));
        }
        sets.push("approval_id = ''");
        sets.push("updated_at = ?");
        values.push(to_rfc3339(now));
        values.push(task_id.to_string());
        values.push(step_id.to_string());
        let sql = format!(
            "UPDATE task_steps SET {} WHERE task_id = ? AND step_id = ? AND (status = 'pending' OR (status = 'running' AND approval_id != ''))",
            sets.join(", ")
        );
        let changed = self
            .db()
            .conn()
            .execute(&sql, hatch_store::rusqlite::params_from_iter(values.iter()))
            .db()?;
        if changed == 0 {
            let step = self.get_step(task_id, step_id)?;
            return match step {
                Some(s) => Err(Error::Invalid(format!(
                    "步骤 {} 已是 {}，不能编辑（只能改未执行或待审批的步骤）",
                    s.step_id,
                    s.status.as_str()
                ))),
                None => Err(Error::NotFound(format!("step {step_id} 不存在"))),
            };
        }
        self.get_step(task_id, step_id)?
            .ok_or_else(|| Error::NotFound(format!("step {step_id} 不存在")))
    }
}
